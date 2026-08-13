use super::*;
use crate::Config;
use crate::cmd::ChatRequest;
use crate::msg::{Key, KeyCode, KeyMods};
use crate::reports::*;
use crate::request::*;
use crate::state::{McpServerEntry, McpState, PendingToolCall, UiState};
use crate::transition::start_executing_tools;
use crate::*;
use mermaid_model::ids::*;
use mermaid_model::models::*;
use std::path::PathBuf;

fn fresh_state() -> State {
    State::new(
        Config::default(),
        PathBuf::from("/tmp/project"),
        "ollama/test".to_string(),
        chrono::Local::now(),
        std::path::PathBuf::from("/tmp"),
    )
}

/// The bug this replaced: provenance used to be stamped by
/// `app::stamp_session_provenance(&mut State, ..)` outside the reducer, so
/// `--replay` re-probed git live and wrote the replaying machine's branch
/// over the recording's. As a `Msg` it replays like everything else — and
/// the reducer only fills blanks, so a session loaded from disk keeps what
/// it was saved with.
#[test]
fn resolved_provenance_fills_blanks_and_never_overwrites() {
    let probed = || crate::SessionProvenance {
        git_branch: Some("replaying-machine".to_string()),
        git_sha: Some("ffffffff".to_string()),
        cli_version: Some("9.9.9".to_string()),
    };

    // A fresh session has none of the three; the probe supplies all.
    let state = fresh_state();
    assert_eq!(state.session.conversation.git_branch, None);
    let (state, cmds) = update(state, Msg::SessionProvenanceResolved(probed()));
    assert!(cmds.is_empty(), "stamping provenance is not an effect");
    assert_eq!(
        state.session.conversation.git_branch.as_deref(),
        Some("replaying-machine")
    );
    assert_eq!(
        state.session.conversation.git_sha.as_deref(),
        Some("ffffffff")
    );
    assert_eq!(
        state.session.conversation.cli_version.as_deref(),
        Some("9.9.9")
    );

    // A resumed session already carries its own; the probe must not win.
    let mut state = fresh_state();
    state.session.conversation.git_branch = Some("recorded-branch".to_string());
    state.session.conversation.git_sha = Some("a614aa9f".to_string());
    state.session.conversation.cli_version = Some("0.21.1".to_string());
    let (state, _) = update(state, Msg::SessionProvenanceResolved(probed()));
    assert_eq!(
        state.session.conversation.git_branch.as_deref(),
        Some("recorded-branch")
    );
    assert_eq!(
        state.session.conversation.git_sha.as_deref(),
        Some("a614aa9f")
    );
    assert_eq!(
        state.session.conversation.cli_version.as_deref(),
        Some("0.21.1")
    );

    // Each field is independent — a detached HEAD records a SHA and no
    // branch, and the blank must still fill.
    let mut state = fresh_state();
    state.session.conversation.git_sha = Some("a614aa9f".to_string());
    let (state, _) = update(state, Msg::SessionProvenanceResolved(probed()));
    assert_eq!(
        state.session.conversation.git_branch.as_deref(),
        Some("replaying-machine")
    );
    assert_eq!(
        state.session.conversation.git_sha.as_deref(),
        Some("a614aa9f")
    );
}

#[test]
fn focus_changed_toggles_terminal_unfocused() {
    let state = fresh_state();
    assert!(!state.ui.terminal_unfocused); // default: assume attended
    let (state, _) = update(state, Msg::FocusChanged(false));
    assert!(state.ui.terminal_unfocused);
    let (state, _) = update(state, Msg::FocusChanged(true));
    assert!(!state.ui.terminal_unfocused);
}

#[test]
fn keyboard_scroll_publishes_deltas_and_end_jumps() {
    let key = |code, modifiers| Msg::Key(Key { code, modifiers });
    let (state, _) = update(fresh_state(), key(KeyCode::PageUp, KeyMods::default()));
    assert_eq!(state.ui.mouse_scroll_accum, 10);
    let (state, _) = update(state, key(KeyCode::PageDown, KeyMods::default()));
    assert_eq!(state.ui.mouse_scroll_accum, 0);
    let shift = KeyMods {
        shift: true,
        ..Default::default()
    };
    let (state, _) = update(state, key(KeyCode::Up, shift));
    assert_eq!(state.ui.mouse_scroll_accum, 1);
    let before = state.ui.scroll_to_bottom_seq;
    let (state, _) = update(state, key(KeyCode::End, KeyMods::default()));
    assert_eq!(state.ui.scroll_to_bottom_seq, before + 1);
}

/// Type each char of `text` through the real reducer.
fn type_text(mut state: State, text: &str) -> (State, Vec<Cmd>) {
    let mut all_cmds = Vec::new();
    for c in text.chars() {
        let (next, cmds) = update(
            state,
            Msg::Key(Key {
                code: KeyCode::Char(c),
                modifiers: KeyMods::default(),
            }),
        );
        state = next;
        all_cmds.extend(cmds);
    }
    (state, all_cmds)
}

fn plain_key(code: KeyCode) -> Msg {
    Msg::Key(Key {
        code,
        modifiers: KeyMods::default(),
    })
}

#[test]
fn file_picker_opens_on_at_and_walks_once() {
    let (state, cmds) = type_text(fresh_state(), "look at @");
    assert!(state.ui.file_picker_open(), "@ opens the picker");
    assert!(state.ui.project_files_loading);
    let walks = cmds
        .iter()
        .filter(|c| matches!(c, Cmd::Query(Query::ListProjectFiles)))
        .count();
    assert_eq!(walks, 1, "open fires exactly one walk");
    // Further filtering while the walk is in flight never re-fires.
    let (state, cmds) = type_text(state, "src");
    assert!(state.ui.file_picker_open());
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::Query(Query::ListProjectFiles))),
        "in-flight walk dedupes"
    );
    let _ = state;
}

#[test]
fn file_picker_ranks_listed_files_and_completes_with_tab() {
    let (state, _) = type_text(fresh_state(), "@ma");
    let (state, _) = update(
        state,
        Msg::QueryResult(QueryResult::ProjectFilesListed(vec![
            "docs/notes.md".to_string(),
            "src/main.rs".to_string(),
        ])),
    );
    assert_eq!(state.ui.file_picker_matches, vec!["src/main.rs"]);
    let (state, _) = update(state, plain_key(KeyCode::Tab));
    assert_eq!(state.ui.input_buffer, "@src/main.rs ");
    assert_eq!(state.ui.input_cursor, state.ui.input_buffer.len());
    assert!(
        !state.ui.file_picker_open(),
        "the trailing space closes the token"
    );
}

/// A paste that lands inside an @-token must re-rank the picker exactly
/// like the keystroke path. The Windows event source coalesces fast
/// keystrokes into `Msg::Paste`, so this is reachable by ordinary typing:
/// the pasted query left the match list stale — still ranked for the
/// pre-paste query — and Enter completed with the wrong file.
#[test]
fn a_paste_into_the_at_token_re_ranks_the_picker() {
    let (state, _) = type_text(fresh_state(), "@");
    let (state, _) = update(
        state,
        Msg::QueryResult(QueryResult::ProjectFilesListed(vec![
            "docs/notes.md".to_string(),
            "src/main.rs".to_string(),
        ])),
    );
    assert_eq!(
        state.ui.file_picker_matches.len(),
        2,
        "empty query ranks everything"
    );
    let (state, _) = update(state, Msg::Paste(Paste::Text("main".to_string())));
    assert_eq!(
        state.ui.file_picker_matches,
        vec!["src/main.rs"],
        "the pasted query narrows the ranking"
    );
    let (state, _) = update(state, plain_key(KeyCode::Enter));
    assert_eq!(
        state.ui.input_buffer, "@src/main.rs ",
        "Enter completes with the narrowed selection, not the stale head"
    );
}

/// A paste that CREATES the token (paste "look at @ma" into an empty
/// composer) opens the picker and fires the walk, same as typing it.
#[test]
fn a_paste_that_creates_the_token_opens_the_picker_and_walks() {
    let (state, cmds) = update(
        fresh_state(),
        Msg::Paste(Paste::Text("look at @ma".to_string())),
    );
    assert!(
        state.ui.file_picker_open(),
        "pasted @-token opens the picker"
    );
    assert_eq!(
        cmds.iter()
            .filter(|c| matches!(c, Cmd::Query(Query::ListProjectFiles)))
            .count(),
        1,
        "the open fires exactly one walk"
    );
}

/// Ctrl+V clipboard text into an @-token follows the same rule as a
/// terminal paste — the two share the insert path and must also agree
/// on re-ranking.
#[test]
fn clipboard_text_into_the_at_token_re_ranks_the_picker() {
    let (state, _) = type_text(fresh_state(), "@");
    let (state, _) = update(
        state,
        Msg::QueryResult(QueryResult::ProjectFilesListed(vec![
            "docs/notes.md".to_string(),
            "src/main.rs".to_string(),
        ])),
    );
    let (state, _) = update(
        state,
        Msg::ClipboardRead(ClipboardRead::Text("main".to_string())),
    );
    assert_eq!(
        state.ui.file_picker_matches,
        vec!["src/main.rs"],
        "clipboard text narrows the ranking"
    );
}

/// Esc dismisses the picker per-token; pasting into the token reopens it,
/// exactly like a keystroke does.
#[test]
fn a_paste_reopens_a_dismissed_picker() {
    let (state, _) = type_text(fresh_state(), "@ma");
    let (state, _) = update(state, plain_key(KeyCode::Escape));
    assert!(!state.ui.file_picker_open(), "Esc dismisses");
    let (state, _) = update(state, Msg::Paste(Paste::Text("in".to_string())));
    assert!(
        state.ui.file_picker_open(),
        "a paste is typing: the dismissal lifts"
    );
}

#[test]
fn ctrl_j_inserts_newline_at_cursor_without_submitting() {
    let (mut state, _) = type_text(fresh_state(), "line one");
    // Move the cursor mid-buffer to prove insertion happens at the
    // cursor, not the end.
    state.ui.input_cursor = 4;
    let (state, cmds) = update(
        state,
        Msg::Key(Key {
            code: KeyCode::Char('j'),
            modifiers: KeyMods::ctrl(),
        }),
    );
    assert_eq!(state.ui.input_buffer, "line\n one");
    assert_eq!(state.ui.input_cursor, 5, "cursor lands after the newline");
    assert!(
        state.session.messages().is_empty(),
        "Ctrl+J must never submit"
    );
    assert!(cmds.is_empty(), "newline insert is reducer-only");
}

#[test]
fn ctrl_j_outside_editing_input_is_ignored() {
    let (mut state, _) = type_text(fresh_state(), "draft");
    state.ui.mode = UiMode::ModelList;
    let (state, _) = update(
        state,
        Msg::Key(Key {
            code: KeyCode::Char('j'),
            modifiers: KeyMods::ctrl(),
        }),
    );
    assert_eq!(
        state.ui.input_buffer, "draft",
        "pickers must not receive a stray newline"
    );
}

#[test]
fn shift_enter_submits_like_plain_enter() {
    let (state, _) = type_text(fresh_state(), "hello there");
    let (state, _) = update(
        state,
        Msg::Key(Key {
            code: KeyCode::Enter,
            modifiers: KeyMods {
                shift: true,
                ..KeyMods::NONE
            },
        }),
    );
    assert!(
        state.ui.input_buffer.is_empty(),
        "Shift+Enter submits — Ctrl+J is the only newline chord"
    );
    assert_eq!(state.session.messages().len(), 1);
}

#[test]
fn file_picker_enter_completes_instead_of_submitting() {
    let (state, _) = type_text(fresh_state(), "@ma");
    let (state, _) = update(
        state,
        Msg::QueryResult(QueryResult::ProjectFilesListed(vec![
            "src/main.rs".to_string(),
        ])),
    );
    let (state, _) = update(state, plain_key(KeyCode::Enter));
    assert_eq!(
        state.ui.input_buffer, "@src/main.rs ",
        "Enter completes the mention"
    );
    assert!(
        state.session.messages().is_empty(),
        "Enter with the picker open must NOT submit the prompt"
    );
}

#[test]
fn file_picker_esc_dismisses_and_typing_reopens() {
    let (state, _) = type_text(fresh_state(), "@ma");
    let (state, _) = update(
        state,
        Msg::QueryResult(QueryResult::ProjectFilesListed(vec![
            "src/main.rs".to_string(),
        ])),
    );
    let buffer_before = state.ui.input_buffer.clone();
    let (state, _) = update(state, plain_key(KeyCode::Escape));
    assert!(!state.ui.file_picker_open(), "Esc dismisses");
    assert_eq!(
        state.ui.input_buffer, buffer_before,
        "Esc leaves the typed text untouched"
    );
    let (state, _) = type_text(state, "i");
    assert!(state.ui.file_picker_open(), "typing reopens the picker");
}

#[test]
fn file_picker_never_opens_on_slash_commands_or_emails() {
    let (state, cmds) = type_text(fresh_state(), "/load @x");
    assert!(!state.ui.file_picker_open(), "slash palette owns `/` input");
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::Query(Query::ListProjectFiles)))
    );
    let (state, cmds) = type_text(fresh_state(), "mail user@host");
    assert!(!state.ui.file_picker_open(), "user@host is not a mention");
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::Query(Query::ListProjectFiles)))
    );
    let _ = state;
}

#[test]
fn file_picker_arrow_keys_move_the_cursor_without_editing() {
    let (state, _) = type_text(fresh_state(), "@s");
    let (state, _) = update(
        state,
        Msg::QueryResult(QueryResult::ProjectFilesListed(vec![
            "src/a.rs".to_string(),
            "src/b.rs".to_string(),
            "src/c.rs".to_string(),
        ])),
    );
    let (state, _) = update(state, plain_key(KeyCode::Down));
    assert_eq!(state.ui.file_picker_cursor, Some(1));
    assert_eq!(state.ui.input_buffer, "@s", "Down never edits the buffer");
    let (state, _) = update(state, plain_key(KeyCode::Up));
    assert_eq!(state.ui.file_picker_cursor, Some(0));
}

/// A conversation with two full user/assistant exchanges.
fn state_with_two_exchanges() -> State {
    let mut state = fresh_state();
    state
        .session
        .append(ChatMessage::user("first prompt"), state.now);
    state
        .session
        .append(ChatMessage::assistant("first reply"), state.now);
    state
        .session
        .append(ChatMessage::user("second prompt"), state.now);
    state
        .session
        .append(ChatMessage::assistant("second reply"), state.now);
    state
}

#[test]
fn double_esc_opens_rewind_picker_single_esc_only_arms() {
    let mut state = state_with_two_exchanges();
    let (state2, _) = update(state.clone(), plain_key(KeyCode::Escape));
    assert!(state2.ui.esc_armed_at.is_some(), "first Esc arms");
    assert!(matches!(state2.ui.mode, UiMode::EditingInput));
    let (state3, _) = update(state2, plain_key(KeyCode::Escape));
    let UiMode::RewindPicker { candidates, cursor } = &state3.ui.mode else {
        panic!("second Esc within the window opens the picker");
    };
    assert_eq!(cursor, &0);
    assert_eq!(candidates.len(), 2, "only user messages are candidates");
    assert_eq!(candidates[0].excerpt, "second prompt", "newest first");
    assert_eq!(candidates[0].message_index, 2);
    assert_eq!(candidates[1].message_index, 0);
    // Past the window: the press RE-ARMS instead of firing.
    state.ui.esc_armed_at = Some(state.now - chrono::Duration::milliseconds(1500));
    let (state4, _) = update(state, plain_key(KeyCode::Escape));
    assert!(matches!(state4.ui.mode, UiMode::EditingInput));
    assert_eq!(state4.ui.esc_armed_at, Some(state4.now), "re-armed");
}

#[test]
fn busy_esc_cancels_and_never_arms() {
    let mut state = state_with_two_exchanges();
    state.turn = start_generating(TurnId(1), std::time::SystemTime::now());
    let (state, cmds) = update(state, plain_key(KeyCode::Escape));
    assert!(
        matches!(state.turn, TurnState::Cancelling { .. }),
        "busy Esc stays the cancel gesture"
    );
    assert!(state.ui.esc_armed_at.is_none(), "busy Esc never arms");
    assert!(cmds.iter().any(|c| matches!(c, Cmd::CancelScope(_))));
}

#[test]
fn any_other_key_disarms_rewind() {
    let state = state_with_two_exchanges();
    let (state, _) = update(state, plain_key(KeyCode::Escape));
    assert!(state.ui.esc_armed_at.is_some());
    let (state, _) = update(state, plain_key(KeyCode::Char('x')));
    assert!(state.ui.esc_armed_at.is_none(), "typing disarms");
}

#[test]
fn double_esc_with_no_user_messages_is_a_noop() {
    let state = fresh_state();
    let (state, _) = update(state, plain_key(KeyCode::Escape));
    let (state, _) = update(state, plain_key(KeyCode::Escape));
    assert!(
        matches!(state.ui.mode, UiMode::EditingInput),
        "nothing to rewind to"
    );
}

#[test]
fn rewind_picker_esc_dismisses_without_mutation() {
    let state = state_with_two_exchanges();
    let original_id = state.session.conversation.id.clone();
    let (state, _) = update(state, plain_key(KeyCode::Escape));
    let (state, _) = update(state, plain_key(KeyCode::Escape));
    assert!(matches!(state.ui.mode, UiMode::RewindPicker { .. }));
    let (state, cmds) = update(state, plain_key(KeyCode::Escape));
    assert!(matches!(state.ui.mode, UiMode::EditingInput));
    assert_eq!(state.session.conversation.id, original_id);
    assert_eq!(state.session.messages().len(), 4, "history untouched");
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::SaveConversation { .. })),
        "dismiss saves nothing"
    );
}

#[test]
fn rewind_enter_forks_with_lineage_and_prefilled_composer() {
    let state = state_with_two_exchanges();
    let original_id = state.session.conversation.id.clone();
    let (state, _) = update(state, plain_key(KeyCode::Escape));
    let (state, _) = update(state, plain_key(KeyCode::Escape));
    // Cursor 0 = "second prompt" (message_index 2).
    let (state, cmds) = update(state, plain_key(KeyCode::Enter));
    let fork = &state.session.conversation;
    assert_ne!(fork.id, original_id, "the fork gets a NEW session id");
    assert_eq!(fork.forked_from.as_deref(), Some(original_id.as_str()));
    assert_eq!(fork.parent_session.as_deref(), Some(original_id.as_str()));
    assert_eq!(
        fork.messages().len(),
        2,
        "fork keeps only the prefix before the selected message"
    );
    assert_eq!(fork.messages()[0].content, "first prompt");
    assert_eq!(fork.messages()[1].content, "first reply");
    assert_eq!(
        state.ui.input_buffer, "second prompt",
        "composer pre-filled with the selected message"
    );
    assert_eq!(state.ui.input_cursor, state.ui.input_buffer.len());
    assert!(matches!(state.ui.mode, UiMode::EditingInput));
    let saves = cmds
        .iter()
        .filter(|c| matches!(c, Cmd::SaveConversation { .. }))
        .count();
    assert_eq!(saves, 2, "original saved first, then the fork");
    // The FIRST save carries the ORIGINAL (full history, its own id).
    let Some(Cmd::SaveConversation {
        snapshot: first_saved,
        ..
    }) = cmds
        .iter()
        .find(|c| matches!(c, Cmd::SaveConversation { .. }))
    else {
        unreachable!()
    };
    assert_eq!(first_saved.id, original_id);
    assert_eq!(first_saved.messages().len(), 4);
}

#[test]
fn rewind_to_first_message_yields_empty_fork_prefix() {
    let state = state_with_two_exchanges();
    let (state, _) = update(state, plain_key(KeyCode::Escape));
    let (state, _) = update(state, plain_key(KeyCode::Escape));
    // Move to the OLDEST candidate ("first prompt", message_index 0).
    let (state, _) = update(state, plain_key(KeyCode::Down));
    let (state, _) = update(state, plain_key(KeyCode::Enter));
    assert!(state.session.messages().is_empty());
    assert_eq!(state.ui.input_buffer, "first prompt");
}

#[test]
fn fork_id_is_a_pure_function_of_the_injected_clock() {
    let build = || {
        let mut s = state_with_two_exchanges();
        s.now = chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05+00:00")
            .unwrap()
            .with_timezone(&chrono::Local);
        let (s, _) = update(s, plain_key(KeyCode::Escape));
        let (s, _) = update(s, plain_key(KeyCode::Escape));
        let (s, _) = update(s, plain_key(KeyCode::Enter));
        s.session.conversation.id.clone()
    };
    assert_eq!(build(), build(), "replay-exact fork ids");
}

#[test]
fn rewind_restages_the_selected_messages_images() {
    let mut state = state_with_two_exchanges();
    let mut msg = ChatMessage::user("look at [Image #1]");
    // 1x1 PNG-ish payload; content only needs to round-trip base64.
    msg.images = Some(vec![base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        b"fake-image-bytes",
    )]);
    msg.image_numbers = Some(vec![1]);
    state.session.append(msg, state.now);
    let (state, _) = update(state, plain_key(KeyCode::Escape));
    let (state, _) = update(state, plain_key(KeyCode::Escape));
    // Cursor 0 = the newest user message (the image one).
    let (state, cmds) = update(state, plain_key(KeyCode::Enter));
    assert_eq!(state.ui.input_buffer, "look at [Image #1]");
    assert_eq!(state.ui.attachments.len(), 1, "image re-staged");
    assert_eq!(state.ui.attachments[0].number, 1, "original number kept");
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::WriteImageToTemp { .. })),
        "temp preview rewritten for the re-staged image"
    );
}

#[test]
fn rewind_candidates_skip_non_user_and_non_normal_messages() {
    let mut state = fresh_state();
    state.session.append(ChatMessage::user("real"), state.now);
    state
        .session
        .append(ChatMessage::system("system note"), state.now);
    let mut summary = ChatMessage::user("summary-ish");
    summary.kind = mermaid_model::models::ChatMessageKind::RunSummary;
    state.session.append(summary, state.now);
    let candidates = rewind_candidates(state.session.messages());
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].excerpt, "real");
}

#[test]
fn desired_title_reflects_run_state() {
    let mut state = fresh_state();
    // Idle: a mermaid title, but not the "working" status.
    assert!(desired_title(&state).starts_with("mermaid"));
    assert!(!desired_title(&state).contains("working"));
    // Active: the working status.
    state.turn = start_generating(TurnId(1), std::time::SystemTime::now());
    assert_eq!(desired_title(&state), "mermaid · working");
}

#[test]
fn help_text_lists_keyboard_shortcuts() {
    let help = help_text(&[]);
    assert!(help.contains("Keyboard shortcuts:"));
    assert!(help.contains("PageUp"));
}

#[test]
fn evict_stale_screenshots_retains_most_recent_and_elides_rest() {
    use mermaid_model::constants::MAX_RETAINED_SCREENSHOTS;
    let mut msgs = Vec::new();
    for i in 0..(MAX_RETAINED_SCREENSHOTS + 3) {
        msgs.push(ChatMessage {
            role: MessageRole::Assistant,
            content: format!("turn {i}"),
            timestamp: chrono::Local::now(),
            kind: mermaid_model::models::ChatMessageKind::Normal,
            metadata: None,
            actions: vec![],
            thinking: None,
            images: Some(vec![format!("png-base64-{}", i)]),
            image_numbers: None,
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            provider_continuation: None,
        });
    }
    let out = crate::request::evict_stale_screenshots(msgs);
    // Last MAX_RETAINED_SCREENSHOTS entries still carry images.
    for m in out.iter().rev().take(MAX_RETAINED_SCREENSHOTS) {
        assert!(m.images.is_some(), "most-recent images must survive");
    }
    // Everything before the cap is elided.
    for m in out.iter().rev().skip(MAX_RETAINED_SCREENSHOTS) {
        assert!(m.images.is_none(), "older images must be elided");
        assert!(
            m.content.contains("elided"),
            "elision marker must land in content"
        );
    }
}

#[test]
fn evict_stale_screenshots_preserves_messages_without_images() {
    use mermaid_model::constants::MAX_RETAINED_SCREENSHOTS;
    // 5 text-only + 2 with images (under the cap) — nothing should
    // be elided.
    let mut msgs = Vec::new();
    for i in 0..5 {
        msgs.push(ChatMessage {
            role: MessageRole::User,
            content: format!("text only {i}"),
            timestamp: chrono::Local::now(),
            kind: mermaid_model::models::ChatMessageKind::Normal,
            metadata: None,
            actions: vec![],
            thinking: None,
            images: None,
            image_numbers: None,
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            provider_continuation: None,
        });
    }
    for i in 0..2 {
        msgs.push(ChatMessage {
            role: MessageRole::Assistant,
            content: format!("with image {i}"),
            timestamp: chrono::Local::now(),
            kind: mermaid_model::models::ChatMessageKind::Normal,
            metadata: None,
            actions: vec![],
            thinking: None,
            images: Some(vec![format!("png-{}", i)]),
            image_numbers: None,
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            provider_continuation: None,
        });
    }
    const { assert!(2 < MAX_RETAINED_SCREENSHOTS, "test premise") };
    let out = crate::request::evict_stale_screenshots(msgs);
    // All 7 messages unchanged.
    let with_images = out.iter().filter(|m| m.images.is_some()).count();
    assert_eq!(with_images, 2);
    assert!(!out.iter().any(|m| m.content.contains("elided")));
}

#[test]
fn quit_sets_exit_flag_and_emits_save_and_exit() {
    let state = fresh_state();
    let (state, cmds) = update(state, Msg::Quit);
    assert!(state.should_exit);
    assert_eq!(cmds.len(), 2);
    assert!(matches!(cmds[0], Cmd::SaveConversation { .. }));
    assert!(matches!(cmds[1], Cmd::Exit));
}

fn ctrl_c() -> Msg {
    Msg::Key(Key {
        code: KeyCode::Char('c'),
        modifiers: KeyMods::ctrl(),
    })
}

/// Press-twice-to-exit: the first Ctrl+C arms the confirm window, the
/// second press inside it exits.
#[test]
fn ctrl_c_on_idle_empty_input_arms_then_second_press_exits() {
    let state = fresh_state();
    let (state, cmds) = update(state, ctrl_c());
    assert!(!state.should_exit, "first press must not exit");
    assert!(cmds.is_empty(), "first press on idle is reducer-only");
    assert!(state.ui.exit_armed_until.is_some(), "first press arms");

    let (state, cmds) = update(state, ctrl_c());
    assert!(state.should_exit, "second press inside the window exits");
    assert!(cmds.iter().any(|c| matches!(c, Cmd::Exit)));
}

/// First Ctrl+C with typed input clears the input (Claude Code parity)
/// instead of exiting; the second press exits.
#[test]
fn ctrl_c_on_idle_with_input_clears_then_second_press_exits() {
    let mut state = fresh_state();
    state.ui.input_buffer = "partial".to_string();
    state.ui.input_cursor = 7;
    let (state, cmds) = update(state, ctrl_c());
    assert!(!state.should_exit);
    assert!(state.ui.input_buffer.is_empty(), "first press clears input");
    assert_eq!(state.ui.input_cursor, 0);
    assert!(cmds.is_empty());

    let (state, cmds) = update(state, ctrl_c());
    assert!(state.should_exit);
    assert!(cmds.iter().any(|c| matches!(c, Cmd::Exit)));
}

/// A second Ctrl+C after the confirm window expired re-arms instead of
/// exiting (lazy expiry against `state.now`).
#[test]
fn armed_exit_expires_after_window() {
    let state = fresh_state();
    let (mut state, _) = update(state, ctrl_c());
    assert!(state.ui.exit_armed_until.is_some());
    // Advance the injected clock past the deadline.
    state.now +=
        chrono::Duration::seconds(mermaid_model::constants::UI_EXIT_CONFIRM_WINDOW_SECS + 1);
    let (state, cmds) = update(state, ctrl_c());
    assert!(!state.should_exit, "expired arm must re-arm, not exit");
    assert!(cmds.is_empty());
    assert!(state.ui.exit_armed_until.is_some(), "re-armed");
}

/// Any other key while armed disarms the exit confirmation.
#[test]
fn any_other_key_disarms_exit_confirmation() {
    let state = fresh_state();
    let (state, _) = update(state, ctrl_c());
    assert!(state.ui.exit_armed_until.is_some());
    let (state, _) = update(state, key(KeyCode::Char('x')));
    assert!(state.ui.exit_armed_until.is_none(), "typing disarms");
    let (state, _) = update(state, ctrl_c());
    assert!(
        !state.should_exit,
        "post-disarm Ctrl+C is a fresh first press"
    );
}

/// Ctrl+Shift+C is the copy chord (kitty-protocol terminals deliver the
/// SHIFT bit) — it must never reach the quit/arm path.
#[test]
fn ctrl_shift_c_does_not_exit_or_arm() {
    let state = fresh_state();
    let msg = Msg::Key(Key {
        code: KeyCode::Char('c'),
        modifiers: KeyMods {
            ctrl: true,
            shift: true,
            ..KeyMods::NONE
        },
    });
    let (state, cmds) = update(state, msg);
    assert!(!state.should_exit);
    assert!(
        state.ui.exit_armed_until.is_none(),
        "copy chord must not arm"
    );
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::Exit)));
}

/// Uppercase 'C' (however delivered) must not match the quit path either.
#[test]
fn ctrl_c_uppercase_never_exits() {
    let state = fresh_state();
    let msg = Msg::Key(Key {
        code: KeyCode::Char('C'),
        modifiers: KeyMods::ctrl(),
    });
    let (state, cmds) = update(state, msg);
    assert!(!state.should_exit);
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::Exit)));
}

/// Tool stdout progress lines must NOT append a chat message. Surfacing
/// every progress line (build output, pids, streamed file contents) as UI
/// would be noise; the status line names the running tool and the full
/// output lands in chat only when the tool finishes.
#[test]
fn tool_progress_output_does_not_append_message() {
    use crate::ProgressEvent;
    let mut state = fresh_state();
    state.turn = start_generating(TurnId(1), std::time::SystemTime::now());
    let turn = state.current_turn_id().unwrap();

    let (state, _cmds) = update(
        state,
        Msg::ToolProgress {
            turn,
            call_id: mermaid_model::ids::ToolCallId(1),
            event: ProgressEvent::Output(
                "drwxrwxr-x  3 nsabaj nsabaj 4096 Mar 30 14:02 .mermaid".to_string(),
            ),
        },
    );
    assert!(
        state.session.messages().is_empty(),
        "tool stdout must not append a chat message"
    );
}

/// F14: Ctrl+V in the chat input emits `Cmd::ReadClipboard`. The
/// reducer stays pure — the actual clipboard read runs off-thread
/// in the effect runner.
#[test]
fn ctrl_v_in_editing_input_emits_read_clipboard() {
    let state = fresh_state();
    assert!(matches!(state.ui.mode, UiMode::EditingInput));
    let (_, cmds) = update(
        state,
        Msg::Key(Key {
            code: KeyCode::Char('v'),
            modifiers: KeyMods::ctrl(),
        }),
    );
    assert!(
        cmds.iter().any(|c| matches!(c, Cmd::ReadClipboard)),
        "Ctrl+V should dispatch Cmd::ReadClipboard; got tags: {:?}",
        cmds.iter().map(|c| c.tag()).collect::<Vec<_>>(),
    );
}

/// F14: Ctrl+V while a confirmation modal is open should NOT
/// hijack the keystroke — the user might be mid-confirmation and
/// accidentally paste into dismissed UI. Gated out.
#[test]
fn ctrl_v_with_confirm_modal_open_is_noop() {
    let mut state = fresh_state();
    state.confirm = Some(crate::state::Confirmation {
        prompt: "Clear conversation history?".to_string(),
        accept_msg_token: crate::state::ConfirmationTarget::ClearConversation,
    });
    let (_, cmds) = update(
        state,
        Msg::Key(Key {
            code: KeyCode::Char('v'),
            modifiers: KeyMods::ctrl(),
        }),
    );
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::ReadClipboard)));
}

/// F14: Ctrl+V in the conversation-list picker must not trigger
/// a clipboard read. The picker has its own key handling.
#[test]
fn ctrl_v_in_conversation_list_mode_is_noop() {
    let mut state = fresh_state();
    state.ui.mode = UiMode::ConversationList {
        candidates: Vec::new(),
        cursor: 0,
    };
    let (_, cmds) = update(
        state,
        Msg::Key(Key {
            code: KeyCode::Char('v'),
            modifiers: KeyMods::ctrl(),
        }),
    );
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::ReadClipboard)));
}

// ── Paste-race guard: Ctrl+V clipboard read vs. a fast Enter ────────

/// Ctrl+V marks a clipboard read in flight so a racing Enter can wait for it.
#[test]
fn ctrl_v_marks_a_clipboard_read_pending() {
    let (state, _) = update(
        fresh_state(),
        Msg::Key(Key {
            code: KeyCode::Char('v'),
            modifiers: KeyMods::ctrl(),
        }),
    );
    assert_eq!(state.ui.clipboard_reads_pending, 1);
}

/// Enter while a clipboard read is still in flight must NOT submit: it holds
/// the submit (so the racing paste isn't dropped) and leaves the buffer intact.
#[test]
fn enter_while_clipboard_read_pending_holds_the_submit() {
    let mut state = fresh_state();
    for c in "hi".chars() {
        let (s, _) = update(state, key(KeyCode::Char(c)));
        state = s;
    }
    // Ctrl+V: a read is now pending.
    let (state, _) = update(
        state,
        Msg::Key(Key {
            code: KeyCode::Char('v'),
            modifiers: KeyMods::ctrl(),
        }),
    );
    // Enter: held, not submitted.
    let (state, cmds) = update(state, key(KeyCode::Enter));
    assert!(state.ui.submit_after_clipboard, "submit is held");
    assert_eq!(
        state.ui.input_buffer, "hi",
        "buffer not consumed while held"
    );
    assert!(
        !cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })),
        "no turn dispatched while a read is pending"
    );
    assert!(
        state
            .session
            .messages()
            .iter()
            .all(|m| m.role != MessageRole::User),
        "no user message sent while the read is pending"
    );
}

/// The full race: paste (read in flight) → Enter → the image lands. The held
/// submit fires exactly once, includes the pasted image, and leaves no stray
/// `[Image #N]` behind in the input.
#[test]
fn held_submit_fires_with_the_pasted_image_once_the_read_lands() {
    let mut state = fresh_state();
    for c in "look".chars() {
        let (s, _) = update(state, key(KeyCode::Char(c)));
        state = s;
    }
    let (state, _) = update(
        state,
        Msg::Key(Key {
            code: KeyCode::Char('v'),
            modifiers: KeyMods::ctrl(),
        }),
    );
    let (state, _) = update(state, key(KeyCode::Enter));
    // The async clipboard read resolves with an image.
    let (state, cmds) = update(
        state,
        Msg::ClipboardRead(crate::msg::ClipboardRead::Image {
            bytes: vec![0x89, 0x50, 0x4E, 0x47],
            format: "png".to_string(),
        }),
    );
    assert_eq!(state.ui.clipboard_reads_pending, 0);
    assert!(!state.ui.submit_after_clipboard, "held submit released");
    let msg = state
        .session
        .messages()
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User)
        .expect("the held submit fires once the image lands");
    assert_eq!(
        msg.images.as_ref().map(Vec::len),
        Some(1),
        "the pasted image is included, not dropped"
    );
    assert_eq!(msg.image_numbers, Some(vec![1]));
    assert!(msg.content.contains("look") && msg.content.contains("[Image #1]"));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })));
    assert!(
        state.ui.attachments.is_empty(),
        "attachment consumed by submit"
    );
    assert!(
        state.ui.input_buffer.is_empty(),
        "no stray token left in the input"
    );
}

/// An empty/failed clipboard read must still release a held submit (never
/// wedge it): the typed text goes out, just without an image.
#[test]
fn empty_clipboard_read_releases_held_submit_without_an_image() {
    let mut state = fresh_state();
    for c in "just text".chars() {
        let (s, _) = update(state, key(KeyCode::Char(c)));
        state = s;
    }
    let (state, _) = update(
        state,
        Msg::Key(Key {
            code: KeyCode::Char('v'),
            modifiers: KeyMods::ctrl(),
        }),
    );
    let (state, _) = update(state, key(KeyCode::Enter));
    let (state, _) = update(state, Msg::ClipboardRead(crate::msg::ClipboardRead::Empty));
    assert_eq!(state.ui.clipboard_reads_pending, 0);
    assert!(!state.ui.submit_after_clipboard);
    let msg = state
        .session
        .messages()
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User)
        .expect("held submit still fires on an empty read");
    assert!(msg.images.is_none(), "no image on an empty read");
    assert!(msg.content.contains("just text"));
}

/// A terminal bracketed paste (`Msg::Paste`) is NOT a Ctrl+V clipboard read:
/// it must not touch the pending counter, which would otherwise let a stray
/// paste prematurely release a held submit.
#[test]
fn bracketed_text_paste_does_not_touch_the_clipboard_counter() {
    let (state, _) = update(
        fresh_state(),
        Msg::Paste(crate::msg::Paste::Text("pasted".to_string())),
    );
    assert_eq!(state.ui.clipboard_reads_pending, 0);
    assert_eq!(state.ui.input_buffer, "pasted");
}

/// Generic async feedback (`Msg::TransientStatus`, e.g. clipboard results)
/// posts a system message into the chat transcript — there is no banner.
#[test]
fn transient_status_posts_to_chat_transcript() {
    let state = fresh_state();
    let (state, cmds) = update(
        state,
        Msg::TransientStatus {
            text: "Clipboard is empty".to_string(),
        },
    );
    let last = state
        .session
        .messages()
        .last()
        .expect("a transcript message was appended");
    assert!(last.content.contains("Clipboard is empty"));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::SaveConversation { .. })),
        "the transcript message is persisted"
    );
}

/// A copy confirmation is feedback on a keystroke, not conversation: it
/// toasts with a deadline and never touches the message log. Routing it
/// through `TransientStatus` left "Copied N chars to clipboard" parked
/// above the input for the rest of the session.
#[test]
fn toast_expires_and_never_enters_the_transcript() {
    let state = fresh_state();
    let now = state.now;
    let (state, cmds) = update(
        state,
        Msg::Toast {
            text: "copied 42 chars to clipboard".to_string(),
        },
    );
    let (text, until) = state.ui.toast.clone().expect("toast is armed");
    assert_eq!(text, "copied 42 chars to clipboard");
    assert_eq!(until, now + crate::state::TOAST_TTL);
    assert!(
        state.session.messages().is_empty(),
        "a toast must not become a transcript row: {:?}",
        state.session.messages()
    );
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::SaveConversation { .. })),
        "a toast is not worth persisting"
    );
    // Expiry is lazy against `state.now` — nothing has to clear it.
    assert!(now <= until, "live immediately after the keystroke");
    assert!(
        now + crate::state::TOAST_TTL + chrono::Duration::milliseconds(1) > until,
        "gone once the TTL passes"
    );
}

// ── No-vision-model warning (Msg::ProviderVisionResolved) ───────────

fn vision_resolved(model_id: &str, supports_vision: Option<bool>, warn: bool) -> Msg {
    Msg::ProviderVisionResolved {
        model_id: model_id.to_string(),
        supports_vision,
        warn,
    }
}

fn count_no_vision_notices(state: &State) -> usize {
    state
        .session
        .messages()
        .iter()
        .filter(|m| m.content.contains("no vision capability"))
        .count()
}

/// A no-vision model with an image in play warns exactly once per session.
#[test]
fn no_vision_model_warns_once() {
    // fresh_state's model is "ollama/test".
    let (state, cmds) = update(
        fresh_state(),
        vision_resolved("ollama/test", Some(false), true),
    );
    assert_eq!(
        count_no_vision_notices(&state),
        1,
        "one warning on first probe"
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::SaveConversation { .. }))
    );
    // A second probe for the same model must not nag again.
    let (state, _) = update(state, vision_resolved("ollama/test", Some(false), true));
    assert_eq!(
        count_no_vision_notices(&state),
        1,
        "warning is once-per-session"
    );
}

/// A vision-capable model refreshes the display snapshot but never warns.
#[test]
fn vision_capable_model_updates_snapshot_without_warning() {
    let state = fresh_state();
    assert!(
        !state.runtime.provider_capabilities.supports_vision,
        "ollama's static default is false"
    );
    let (state, _) = update(state, vision_resolved("ollama/test", Some(true), true));
    assert!(
        state.runtime.provider_capabilities.supports_vision,
        "snapshot refreshed to the probed value"
    );
    assert_eq!(count_no_vision_notices(&state), 0);
}

/// Unknown vision (`None` — non-Ollama or a failed probe) is ignored: no
/// warning and no snapshot change.
#[test]
fn unknown_vision_is_ignored() {
    let (state, _) = update(fresh_state(), vision_resolved("ollama/test", None, true));
    assert!(
        !state.runtime.provider_capabilities.supports_vision,
        "snapshot untouched on unknown"
    );
    assert_eq!(count_no_vision_notices(&state), 0);
}

/// `warn: false` (no image in play) suppresses the nag even for a no-vision
/// model — the probe is only keeping the snapshot honest.
#[test]
fn no_warn_flag_suppresses_the_nag() {
    let (state, _) = update(
        fresh_state(),
        vision_resolved("ollama/test", Some(false), false),
    );
    assert_eq!(count_no_vision_notices(&state), 0);
}

/// A probe that lands after a `/model` switch (`model_id` no longer matches the
/// active model) is dropped — no warning for the model now in use.
#[test]
fn stale_vision_probe_is_dropped() {
    let (state, _) = update(
        fresh_state(),
        vision_resolved("ollama/previous", Some(false), true),
    );
    assert_eq!(count_no_vision_notices(&state), 0);
}

/// Staging a pasted image proactively probes vision so the warning can appear
/// before the user sends.
#[test]
fn staging_an_image_probes_vision() {
    let (_, cmds) = update(
        fresh_state(),
        Msg::ClipboardRead(crate::msg::ClipboardRead::Image {
            bytes: vec![1, 2, 3],
            format: "png".to_string(),
        }),
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::ProbeVision { warn: true, .. })),
        "pasting an image probes vision with warn=true"
    );
}

/// Switching models probes the new model's vision; it only arms the warning
/// (`warn: true`) when an image is already staged.
#[test]
fn model_switch_probes_vision_and_arms_warning_only_with_staged_image() {
    // No image staged → probe with warn=false (snapshot refresh only).
    let (_, cmds) = update(
        fresh_state(),
        Msg::Slash(SlashCmd::Model(Some("ollama/other".to_string()))),
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::ProbeVision { warn: false, .. })),
        "switching with no staged image probes with warn=false"
    );
    // Stage an image, then switch → probe with warn=true.
    let (state, _) = update(
        fresh_state(),
        Msg::ClipboardRead(crate::msg::ClipboardRead::Image {
            bytes: vec![9],
            format: "png".to_string(),
        }),
    );
    let (_, cmds) = update(
        state,
        Msg::Slash(SlashCmd::Model(Some("ollama/other".to_string()))),
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::ProbeVision { warn: true, .. })),
        "switching with a staged image arms the warning"
    );
}

/// F14: a `Msg::ClipboardRead(Image)` (the Ctrl+V clipboard read result)
/// creates an Attachment entry and emits `Cmd::WriteImageToTemp`. This is the
/// existing contract; the test pins it so the Ctrl+V wiring has a
/// known-good downstream to rely on.
#[test]
fn paste_image_creates_attachment_and_writes_temp() {
    let state = fresh_state();
    let (state, cmds) = update(
        state,
        Msg::ClipboardRead(crate::msg::ClipboardRead::Image {
            bytes: vec![0x89, 0x50, 0x4E, 0x47], // PNG magic bytes
            format: "png".to_string(),
        }),
    );
    assert_eq!(state.ui.attachments.len(), 1);
    let att = &state.ui.attachments[0];
    assert_eq!(att.format, "png");
    assert_eq!(att.size_bytes, 4);
    // First paste mints global image #1, splices the inline pill into the
    // buffer, and advances the cursor past it.
    assert_eq!(att.number, 1);
    assert_eq!(state.ui.input_buffer, "[Image #1] ");
    assert_eq!(state.ui.input_cursor, "[Image #1] ".len());
    assert!(
        cmds.iter()
            .any(|c| { matches!(c, Cmd::WriteImageToTemp { path, .. } if path == &att.temp_path) })
    );
}

#[test]
fn atomic_backspace_deletes_whole_pill_and_its_attachment() {
    let (state, _) = update(
        fresh_state(),
        Msg::ClipboardRead(crate::msg::ClipboardRead::Image {
            bytes: vec![1, 2, 3, 4],
            format: "png".to_string(),
        }),
    );
    assert_eq!(state.ui.input_buffer, "[Image #1] ");
    // Backspace #1: normal delete of the trailing space; pill intact.
    let (state, _) = update(
        state,
        Msg::Key(Key {
            code: KeyCode::Backspace,
            modifiers: KeyMods::default(),
        }),
    );
    assert_eq!(state.ui.input_buffer, "[Image #1]");
    assert_eq!(state.ui.attachments.len(), 1, "pill intact → image intact");
    // Backspace #2: cursor now abuts the pill → whole pill + image removed.
    let (state, _) = update(
        state,
        Msg::Key(Key {
            code: KeyCode::Backspace,
            modifiers: KeyMods::default(),
        }),
    );
    assert_eq!(state.ui.input_buffer, "");
    assert!(state.ui.attachments.is_empty(), "pill gone → image gone");
}

#[test]
fn submit_sends_images_in_token_order_and_drops_orphans() {
    let mut state = fresh_state();
    state.ui.attachments.push(test_attachment(2)); // id 2, number 2
    state.ui.attachments.push(test_attachment(1)); // id 1, number 1
    // References #2 before #1, plus a phantom #9 that owns no attachment.
    let (state, _) = update(
        state,
        Msg::SubmitPrompt {
            text: "[Image #2] a [Image #1] b [Image #9]".to_string(),
            attachment_ids: vec![1, 2],
        },
    );
    let msg = state
        .session
        .messages()
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User)
        .expect("submitted user message");
    // First-appearance order (#2 then #1); the phantom #9 sends no image.
    assert_eq!(msg.image_numbers, Some(vec![2, 1]));
    assert_eq!(msg.images.as_ref().map(Vec::len), Some(2));
    assert!(
        state.ui.attachments.is_empty(),
        "owned attachments consumed / GC'd"
    );
}

#[test]
fn submit_with_typed_literal_and_no_attachment_sends_no_image() {
    let (state, _) = update(
        fresh_state(),
        Msg::SubmitPrompt {
            text: "compare [Image #99] please".to_string(),
            attachment_ids: vec![],
        },
    );
    let msg = state
        .session
        .messages()
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User)
        .expect("submitted user message");
    assert!(msg.images.is_none());
    assert!(
        msg.content.contains("[Image #99]"),
        "the literal stays in the text"
    );
}

#[test]
fn image_numbering_is_global_and_monotonic_across_messages() {
    // Message 1: paste (→ #1) and submit.
    let (state, _) = update(
        fresh_state(),
        Msg::ClipboardRead(crate::msg::ClipboardRead::Image {
            bytes: vec![1],
            format: "png".to_string(),
        }),
    );
    assert_eq!(state.ui.attachments[0].number, 1);
    let text1 = state.ui.input_buffer.clone();
    let ids1: Vec<u64> = state.ui.attachments.iter().map(|a| a.id).collect();
    let (state, _) = update(
        state,
        Msg::SubmitPrompt {
            text: text1,
            attachment_ids: ids1,
        },
    );
    assert_eq!(
        state
            .session
            .messages()
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User)
            .unwrap()
            .image_numbers,
        Some(vec![1])
    );
    // Message 2: the next paste keeps climbing to #2 (global, not per-message).
    let (state, _) = update(
        state,
        Msg::ClipboardRead(crate::msg::ClipboardRead::Image {
            bytes: vec![2],
            format: "png".to_string(),
        }),
    );
    assert_eq!(state.ui.attachments[0].number, 2);
    assert_eq!(state.ui.input_buffer, "[Image #2] ");
}

#[test]
fn resume_continues_image_numbering_past_transcript_max() {
    let mut state = fresh_state();
    let mut history =
        crate::ConversationHistory::new("proj".to_string(), "model".to_string(), state.now);
    history
        .messages_mut()
        .push(ChatMessage::user("look [Image #16]").with_image_numbers(vec![16]));
    state.seed_conversation(history);
    // A paste after resume continues past the transcript's #16 → #17, not #1.
    let (state, _) = update(
        state,
        Msg::ClipboardRead(crate::msg::ClipboardRead::Image {
            bytes: vec![1],
            format: "png".to_string(),
        }),
    );
    assert_eq!(state.ui.attachments[0].number, 17);
    assert_eq!(state.ui.input_buffer, "[Image #17] ");
}

#[test]
fn open_image_writes_and_opens_the_same_temp_path() {
    let mut state = fresh_state();
    let image = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"image bytes");
    state.session.append(
        ChatMessage::assistant("image").with_images(vec![image]),
        state.now,
    );

    let (_, cmds) = update(
        state,
        Msg::OpenImageAt {
            message_index: 0,
            image_index: 0,
            image_number: None,
        },
    );

    let write_path = cmds.iter().find_map(|cmd| match cmd {
        Cmd::WriteImageToTemp { path, .. } => Some(path.clone()),
        _ => None,
    });
    let open_path = cmds.iter().find_map(|cmd| match cmd {
        Cmd::OpenInSystem(path) => Some(path.clone()),
        _ => None,
    });
    assert_eq!(write_path, open_path);
}

#[test]
fn open_image_resolves_by_global_number_over_stale_position() {
    // The click map indexes the DISPLAY transcript, which the continuation
    // stitch can shift away from committed history. The stable [Image #N]
    // number must win over a stale positional pair.
    use base64::Engine as _;
    let mut state = fresh_state();
    let first = base64::engine::general_purpose::STANDARD.encode(b"first image");
    let second = base64::engine::general_purpose::STANDARD.encode(b"second image");
    state.session.append(
        ChatMessage::user("a [Image #7]")
            .with_images(vec![first])
            .with_image_numbers(vec![7]),
        state.now,
    );
    state.session.append(
        ChatMessage::user("b [Image #9]")
            .with_images(vec![second.clone()])
            .with_image_numbers(vec![9]),
        state.now,
    );

    // Positional pair deliberately points at the FIRST message.
    let (_, cmds) = update(
        state,
        Msg::OpenImageAt {
            message_index: 0,
            image_index: 0,
            image_number: Some(9),
        },
    );

    let expected = base64::engine::general_purpose::STANDARD
        .decode(second)
        .unwrap();
    let written = cmds.iter().find_map(|cmd| match cmd {
        Cmd::WriteImageToTemp { bytes, .. } => Some(bytes.clone()),
        _ => None,
    });
    assert_eq!(
        written.as_deref(),
        Some(expected.as_slice()),
        "resolution follows the global number, not the display position"
    );
}

#[test]
fn ctrl_c_during_turn_cancels_then_second_press_exits() {
    let mut state = fresh_state();
    state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
    // First press: interrupt the running turn (like Esc), don't exit.
    let (state, cmds) = update(state, ctrl_c());
    assert!(!state.should_exit, "first press interrupts, not exits");
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::CancelScope(TurnId(5))))
    );
    assert!(matches!(state.turn, TurnState::Cancelling { .. }));
    // Second press inside the window: exit for real.
    let (state, cmds) = update(state, ctrl_c());
    assert!(state.should_exit);
    assert!(cmds.iter().any(|c| matches!(c, Cmd::Exit)));
}

#[test]
fn cancel_and_reset_paths_clear_pending_question() {
    // RC-1 (D2/D3/D4): a parked `ask_user_question` modal must not survive a
    // turn cancel/reset — the tool task behind it is torn down, so the modal
    // would be permanently unanswerable. Every cancel/reset path clears it.
    use mermaid_model::ids::ToolCallId;

    let parked = || {
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
        let (state, _) = update(
            state,
            Msg::QuestionAsked {
                turn: TurnId(5),
                call_id: ToolCallId(1),
                questions: vec![],
            },
        );
        assert_eq!(
            state.pending_question.len(),
            1,
            "precondition: a question is parked mid-turn"
        );
        state
    };

    // Esc / CancelTurn.
    let (state, _) = update(parked(), Msg::CancelTurn);
    assert!(
        state.pending_question.is_empty(),
        "CancelTurn must clear the parked question"
    );

    // Ctrl+C quit (request_exit).
    let (state, _) = update(
        parked(),
        Msg::Key(Key {
            code: KeyCode::Char('c'),
            modifiers: KeyMods::ctrl(),
        }),
    );
    assert!(
        state.pending_question.is_empty(),
        "Ctrl+C quit must clear the parked question"
    );

    // `/load` a conversation mid-turn.
    let history = fresh_state().session.conversation.clone();
    let (state, _) = update(
        parked(),
        Msg::QueryResult(QueryResult::ConversationLoaded(Box::new(history))),
    );
    assert!(
        state.pending_question.is_empty(),
        "ConversationLoaded must clear the parked question"
    );

    // `/clear` (confirmed) mid-turn.
    let mut state = parked();
    state.confirm = Some(crate::state::Confirmation {
        prompt: "Clear conversation history?".to_string(),
        accept_msg_token: crate::state::ConfirmationTarget::ClearConversation,
    });
    let (state, _) = update(state, Msg::ConfirmAccepted);
    assert!(
        state.pending_question.is_empty(),
        "ClearConversation must clear the parked question"
    );
}

#[test]
fn load_conversation_mid_turn_cancels_orphaned_scope() {
    // `/load` while a turn is generating must cancel the in-flight scope,
    // not silently overwrite `state.turn` and orphan the running tasks (#2).
    let mut state = fresh_state();
    state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
    let history = fresh_state().session.conversation.clone();
    let (state, cmds) = update(
        state,
        Msg::QueryResult(QueryResult::ConversationLoaded(Box::new(history))),
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::CancelScope(TurnId(5)))),
        "loading a conversation mid-turn must cancel the in-flight scope"
    );
    assert!(matches!(state.turn, TurnState::Idle));
}

#[test]
fn load_conversation_when_idle_does_not_cancel() {
    // No in-flight turn → nothing to cancel; `/load` just swaps state.
    let state = fresh_state();
    let history = fresh_state().session.conversation.clone();
    let (state, cmds) = update(
        state,
        Msg::QueryResult(QueryResult::ConversationLoaded(Box::new(history))),
    );
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::CancelScope(_))));
    assert!(matches!(state.turn, TurnState::Idle));
}

#[test]
fn scratchpad_ready_stamps_the_matching_session() {
    let state = fresh_state();
    let id = state.session.conversation.id.clone();
    let path = std::path::PathBuf::from("/data/tmp/scratchpad/-proj/x");
    let (state, cmds) = update(
        state,
        Msg::ScratchpadReady {
            session_id: id,
            path: path.clone(),
        },
    );
    assert_eq!(state.session.scratchpad.as_deref(), Some(path.as_path()));
    assert!(cmds.is_empty(), "stamping is silent");
}

#[test]
fn scratchpad_ready_for_a_stale_session_is_dropped() {
    // A `/clear` or `/load` racing the effect's mkdir leaves a ready for
    // a discarded conversation id — it must not attach to the new one.
    let state = fresh_state();
    let (state, _) = update(
        state,
        Msg::ScratchpadReady {
            session_id: "some_other_session".to_string(),
            path: std::path::PathBuf::from("/data/tmp/scratchpad/-proj/stale"),
        },
    );
    assert_eq!(state.session.scratchpad, None);
}

#[test]
fn clear_recomputes_the_scratchpad_for_the_new_conversation_id() {
    let mut state = fresh_state();
    let old_id = state.session.conversation.id.clone();
    state.session.scratchpad = Some(std::path::PathBuf::from("/data/tmp/scratchpad/-proj/old"));
    // Ids are minted from `state.now`; advance it so the cleared
    // conversation provably gets a different id than the original.
    state.now += chrono::Duration::seconds(1);
    state.confirm = Some(crate::state::Confirmation {
        prompt: "Clear conversation history?".to_string(),
        accept_msg_token: crate::state::ConfirmationTarget::ClearConversation,
    });
    let (state, cmds) = update(state, Msg::ConfirmAccepted);
    let new_id = state.session.conversation.id.clone();
    assert_ne!(new_id, old_id);
    assert_eq!(
        state.session.scratchpad, None,
        "the old session's scratch dir must not leak into the new one"
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::EnsureScratchpad { session_id } if *session_id == new_id)),
        "clear must request a scratch dir keyed by the NEW conversation id"
    );
}

#[test]
fn slash_scratchpad_lists_the_stamped_dir_or_explains_its_absence() {
    // Stamped: the listing runs as an effect against the stamped path.
    let mut state = fresh_state();
    let path = std::path::PathBuf::from("/data/tmp/scratchpad/-proj/s");
    state.session.scratchpad = Some(path.clone());
    let (_, cmds) = update(state, Msg::Slash(SlashCmd::Scratchpad));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::ListScratchpad { path: p } if *p == path)),
        "expected ListScratchpad carrying the stamped path, got {cmds:?}"
    );
    // Not stamped (`fresh_state` starts with `scratchpad: None`): a
    // system message instead of a doomed effect.
    let state = fresh_state();
    let (state, cmds) = update(state, Msg::Slash(SlashCmd::Scratchpad));
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::ListScratchpad { .. })));
    let msg = state.session.messages().last().expect("system message");
    assert!(msg.content.contains("No scratchpad yet"), "{}", msg.content);
}

#[test]
fn doctor_reports_the_scratchpad_path() {
    let mut state = fresh_state();
    state.session.scratchpad = Some(std::path::PathBuf::from("/data/tmp/scratchpad/-proj/s"));
    let (state, _) = update(state, Msg::Slash(SlashCmd::Doctor));
    let report = &state.session.messages().last().expect("report").content;
    assert!(
        report.contains("Scratchpad: /data/tmp/scratchpad/-proj/s"),
        "{report}"
    );
    // Before the ready lands, /doctor says so instead of omitting the line.
    let state = fresh_state();
    let (state, _) = update(state, Msg::Slash(SlashCmd::Doctor));
    let report = &state.session.messages().last().expect("report").content;
    assert!(report.contains("Scratchpad: not ready"), "{report}");
}

#[test]
fn load_conversation_recomputes_the_scratchpad() {
    let mut state = fresh_state();
    state.session.scratchpad = Some(std::path::PathBuf::from("/data/tmp/scratchpad/-proj/old"));
    let mut history = fresh_state().session.conversation.clone();
    history.id = "loaded_session".to_string();
    let (state, cmds) = update(
        state,
        Msg::QueryResult(QueryResult::ConversationLoaded(Box::new(history))),
    );
    assert_eq!(state.session.scratchpad, None);
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::EnsureScratchpad { session_id } if session_id == "loaded_session"
    )));
}

#[test]
fn clear_conversation_mid_turn_cancels_scope_and_resets_turn() {
    // F34: confirming `/clear` while a turn is generating must cancel the
    // in-flight scope and reset to Idle (mirroring `ConversationLoaded`), so
    // the orphaned model/tool tasks stop and a stray same-id
    // `StreamDone`/`ToolFinished` can't commit into the cleared conversation.
    let mut state = fresh_state();
    state
        .session
        .append(ChatMessage::user("scratch history"), state.now);
    state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
    state.confirm = Some(crate::state::Confirmation {
        prompt: "Clear conversation history?".to_string(),
        accept_msg_token: crate::state::ConfirmationTarget::ClearConversation,
    });

    let (state, cmds) = update(state, Msg::ConfirmAccepted);

    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::CancelScope(TurnId(5)))),
        "clearing mid-turn must cancel the in-flight scope (F34)"
    );
    assert!(
        matches!(state.turn, TurnState::Idle),
        "turn must reset to Idle after clear"
    );
    assert!(
        state.session.messages().is_empty(),
        "clear must wipe to a fresh, empty conversation"
    );
}

#[test]
fn clear_conversation_when_idle_does_not_cancel() {
    // No in-flight turn → nothing to cancel; clear just wipes the history.
    let mut state = fresh_state();
    state.confirm = Some(crate::state::Confirmation {
        prompt: "Clear conversation history?".to_string(),
        accept_msg_token: crate::state::ConfirmationTarget::ClearConversation,
    });
    let (state, cmds) = update(state, Msg::ConfirmAccepted);
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::CancelScope(_))));
    assert!(matches!(state.turn, TurnState::Idle));
}

/// Build an `ExecutingTools` state with one outstanding call plus the
/// committed `assistant(tool_calls)` that a real turn leaves as the trailing
/// message before the tool results land.
fn executing_tools_with_committed_call(turn: TurnId) -> State {
    let mut state = fresh_state();
    let source = mermaid_model::models::tool_call::ToolCall {
        id: Some("call-1".to_string()),
        function: mermaid_model::models::tool_call::FunctionCall {
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "foo"}),
        },
    };
    let call = PendingToolCall {
        call_id: mermaid_model::ids::ToolCallId(1),
        source: source.clone(),
    };
    state.session.append(
        ChatMessage::assistant("running a tool").with_tool_calls(vec![source]),
        state.now,
    );
    state.turn = start_executing_tools(turn, vec![call], std::time::SystemTime::now());
    state
}

#[test]
fn cancel_mid_tools_seals_orphaned_tool_calls() {
    // Cancelling while tools run left the committed `assistant(tool_calls)`
    // without matching `tool` results — the next request then 400s on
    // Anthropic ("tool_use without tool_result"). The cancel path must now
    // seal every outstanding call with a cancelled placeholder.
    let state = executing_tools_with_committed_call(TurnId(7));
    let (state, _cmds) = update(state, Msg::CancelTurn);
    // History is well-formed the moment we leave ExecutingTools.
    let last = state.session.messages().last().expect("a message");
    assert_eq!(
        last.role,
        MessageRole::Tool,
        "the orphaned tool_call must be sealed with a tool result"
    );
    assert_eq!(last.tool_call_id.as_deref(), Some("call-1"));
    assert!(matches!(state.turn, TurnState::Cancelling { .. }));

    // The terminal TurnCancelled still closes the turn out to Idle.
    let (state, _cmds) = update(state, Msg::TurnCancelled(TurnId(7)));
    assert!(matches!(state.turn, TurnState::Idle));
}

#[test]
fn quit_mid_tools_seals_orphaned_tool_calls_before_saving() {
    // Same hazard on the quit path: the saved history a later `--continue`
    // reloads must not be a dangling `assistant(tool_calls)`.
    let state = executing_tools_with_committed_call(TurnId(7));
    let (state, cmds) = update(state, Msg::Quit);
    assert!(state.should_exit);
    let last = state.session.messages().last().expect("a message");
    assert_eq!(last.role, MessageRole::Tool);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::SaveConversation { .. }))
    );
}

#[test]
fn system_note_during_tools_does_not_split_tool_pair() {
    // A system note appended between `assistant(tool_calls)` and its results
    // wedges a message into the tool_use/tool_result pair, which OpenAI- and
    // Ollama-shaped providers reject. `push_system` must insert it *before*
    // the trailing assistant message instead, keeping the pair adjacent and
    // the assistant message last (so tool actions still attach to it).
    let mut state = executing_tools_with_committed_call(TurnId(7));
    let mut cmds = Vec::new();
    push_system(&mut state, &mut cmds, "an MCP server errored mid-turn");

    let msgs = state.session.messages();
    let last = msgs.last().expect("a message");
    assert_eq!(
        last.role,
        MessageRole::Assistant,
        "the assistant(tool_calls) must stay last so results follow it directly"
    );
    assert!(last.tool_calls.is_some());
    let prev = &msgs[msgs.len() - 2];
    assert_eq!(prev.role, MessageRole::System);
    assert!(prev.content.contains("MCP server errored"));
}

#[test]
fn system_note_when_idle_appends_normally() {
    // Outside ExecutingTools the note is a plain append (no trailing
    // tool-call message to protect).
    let mut state = fresh_state();
    state.session.append(ChatMessage::user("hi"), state.now);
    let mut cmds = Vec::new();
    push_system(&mut state, &mut cmds, "just a note");
    let last = state.session.messages().last().expect("a message");
    assert_eq!(last.role, MessageRole::System);
    assert!(last.content.contains("just a note"));
}

#[test]
fn load_conversation_drops_queued_messages() {
    // A message queued against the previous conversation must not survive a
    // `/load` and auto-submit into the loaded one.
    let mut state = fresh_state();
    state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
    state
        .ui
        .queued_messages
        .push_back(crate::state::QueuedMessage {
            text: "stale queued prompt".to_string(),
            attachment_ids: Vec::new(),
        });
    let history = fresh_state().session.conversation.clone();
    let (state, _cmds) = update(
        state,
        Msg::QueryResult(QueryResult::ConversationLoaded(Box::new(history))),
    );
    assert!(
        state.ui.queued_messages.is_empty(),
        "queued messages must be dropped on /load"
    );
}

#[test]
fn clear_conversation_drops_queued_messages() {
    // Same for `/clear`: a mid-turn queued message belonged to the wiped
    // conversation and must not auto-submit into the fresh one.
    let mut state = fresh_state();
    state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
    state
        .ui
        .queued_messages
        .push_back(crate::state::QueuedMessage {
            text: "stale queued prompt".to_string(),
            attachment_ids: Vec::new(),
        });
    state.confirm = Some(crate::state::Confirmation {
        prompt: "Clear conversation history?".to_string(),
        accept_msg_token: crate::state::ConfirmationTarget::ClearConversation,
    });
    let (state, _cmds) = update(state, Msg::ConfirmAccepted);
    assert!(
        state.ui.queued_messages.is_empty(),
        "queued messages must be dropped on /clear"
    );
}

#[test]
fn upstream_error_during_cancelling_is_dropped() {
    // F35: a late `UpstreamError` from a cancelled provider call (same turn
    // id, state already `Cancelling`) must be a no-op — not paint a spurious
    // error line for the user's own cancel, and not drain a queued message
    // early (which would race the terminal `TurnCancelled` from `drop_scope`).
    let mut state = fresh_state();
    state.turn = TurnState::Cancelling {
        id: TurnId(5),
        since: std::time::SystemTime::now(),
    };
    let before_len = state.session.messages().len();

    let (state, cmds) = update(
        state,
        Msg::UpstreamError {
            turn: TurnId(5),
            error: mermaid_model::models::UserFacingError {
                summary: "Backend error".to_string(),
                message: "connection reset".to_string(),
                suggestion: String::new(),
                category: mermaid_model::models::ErrorCategory::Connection,
                recoverable: true,
            },
        },
    );

    assert!(
        matches!(state.turn, TurnState::Cancelling { id: TurnId(5), .. }),
        "the turn must stay Cancelling until TurnCancelled lands"
    );
    assert_eq!(
        state.session.messages().len(),
        before_len,
        "no error message should be committed for the user's own cancel"
    );
    assert!(
        cmds.is_empty(),
        "a dropped cancel-side-channel error emits no commands"
    );
}

#[test]
fn reducer_reads_injected_now_not_wall_clock() {
    // Cause 3 determinism: the reducer stamps turn timestamps from
    // `state.now`, never `SystemTime::now()` / `Local::now()`. Folding the
    // same `(State, Msg)` with the same injected `now` must produce a
    // byte-identical `since`, and it must equal the injected value — proving
    // the reducer is a pure function of its inputs and a replay fold
    // reproduces State exactly.
    let now = chrono::Local::now();
    let cancel_since = |injected: chrono::DateTime<chrono::Local>| {
        let mut state = fresh_state();
        state.now = injected;
        state.turn = start_generating(TurnId(1), std::time::SystemTime::from(injected));
        let (state, cmds) = update(state, Msg::CancelTurn);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::CancelScope(TurnId(1))))
        );
        match state.turn {
            TurnState::Cancelling { since, .. } => since,
            other => panic!("expected Cancelling, got {other:?}"),
        }
    };
    // Same injected clock ⇒ identical result, regardless of real wall time.
    assert_eq!(cancel_since(now), cancel_since(now));
    // And the stamp is exactly the injected value, not "roughly now".
    assert_eq!(cancel_since(now), std::time::SystemTime::from(now));
    // A different injected clock yields a correspondingly different stamp.
    let earlier = now - chrono::Duration::seconds(3600);
    assert_eq!(cancel_since(earlier), std::time::SystemTime::from(earlier));
    assert_ne!(cancel_since(earlier), cancel_since(now));
}

#[test]
fn runtime_signal_exits_and_records_timeline() {
    let state = fresh_state();
    let (state, cmds) = update(
        state,
        Msg::RuntimeSignal(crate::runtime::RuntimeSignal::Terminate),
    );
    assert!(state.should_exit);
    assert!(cmds.iter().any(|c| matches!(c, Cmd::Exit)));
    assert!(
        state
            .runtime
            .timeline
            .iter()
            .any(|event| event.message.contains("terminate"))
    );
}

#[test]
fn model_switch_updates_provider_capability_snapshot() {
    let state = fresh_state();
    let (state, cmds) = update(
        state,
        Msg::Slash(SlashCmd::Model(Some(
            "anthropic/claude-opus-4-7".to_string(),
        ))),
    );
    assert_eq!(state.runtime.provider_capabilities.provider, "anthropic");
    assert!(state.runtime.provider_capabilities.supports_vision);
    assert!(cmds.iter().any(|c| matches!(c, Cmd::PersistLastModel(_))));
}

#[test]
fn build_chat_request_preserves_explicit_zero_temperature() {
    // D5: an explicit temperature of 0.0 (deterministic decoding) must reach
    // the request as 0.0, not be silently clobbered to DEFAULT_TEMPERATURE.
    let mut state = fresh_state();
    state.settings.default_model.temperature = 0.0;
    assert_eq!(build_chat_request(&state).temperature, 0.0);
}

#[test]
fn hook_context_buffers_caps_and_clears_on_dispatch() {
    let mut state = fresh_state();
    state.turn = start_generating(TurnId(1), std::time::SystemTime::now());
    // Buffer context for the CURRENT turn…
    let (state, _) = update(
        state,
        Msg::HookContext {
            turn: TurnId(1),
            texts: vec!["remember: staging only".to_string()],
        },
    );
    assert_eq!(state.pending_hook_context.len(), 1);
    // …a stale turn's context is dropped (defense in depth + stale filter)…
    let (mut state2, _) = update(
        state,
        Msg::HookContext {
            turn: TurnId(999),
            texts: vec!["stale".to_string()],
        },
    );
    assert_eq!(state2.pending_hook_context.len(), 1);
    // …the byte cap bounds the buffer…
    let big = "x".repeat(super::MAX_HOOK_CONTEXT_BYTES);
    super::handle_hook_context(&mut state2, TurnId(1), vec![big]);
    assert_eq!(
        state2.pending_hook_context.len(),
        1,
        "over-cap context must be dropped"
    );
    // …the request carries the hook block and dispatch consumes it once.
    let mut cmds = Vec::new();
    super::push_call_model(&mut state2, &mut cmds, TurnId(2));
    let Some(Cmd::CallModel { request, .. }) =
        cmds.iter().find(|c| matches!(c, Cmd::CallModel { .. }))
    else {
        panic!("expected a CallModel");
    };
    let instr = request.instructions.clone().expect("instructions present");
    assert!(instr.contains("# Hook Context"));
    assert!(instr.contains("remember: staging only"));
    assert!(
        state2.pending_hook_context.is_empty(),
        "dispatch must consume the buffer"
    );
    // A second dispatch has no hook block.
    let mut cmds = Vec::new();
    super::push_call_model(&mut state2, &mut cmds, TurnId(3));
    let Some(Cmd::CallModel { request, .. }) =
        cmds.iter().find(|c| matches!(c, Cmd::CallModel { .. }))
    else {
        panic!("expected a CallModel");
    };
    assert!(
        !request
            .instructions
            .clone()
            .unwrap_or_default()
            .contains("# Hook Context")
    );
}

#[test]
fn build_chat_request_injects_memory_index() {
    let mut state = fresh_state();
    // No memory loaded → no memory block in the dynamic suffix.
    assert!(
        !build_chat_request(&state)
            .instructions
            .map(|i| i.contains("# Memory"))
            .unwrap_or(false)
    );
    // With memory → the auto-derived index is composed into the suffix.
    state.memory = Some(crate::LoadedMemory {
        entries: Vec::new(),
        index: "# Memory\n\n## Global (all projects)\n- [pnpm] use pnpm — /m/pnpm.md\n".to_string(),
        truncated: false,
    });
    let instr = build_chat_request(&state)
        .instructions
        .expect("memory index should populate the instructions suffix");
    assert!(instr.contains("# Memory"));
    assert!(instr.contains("[pnpm] use pnpm"));
}

#[test]
fn build_chat_request_injects_skill_index() {
    let mut state = fresh_state();
    // No skills discovered → no skills block in the dynamic suffix.
    assert!(
        !build_chat_request(&state)
            .instructions
            .map(|i| i.contains("# Skills"))
            .unwrap_or(false)
    );
    // With skills → the pre-rendered index is composed into the suffix.
    state.skills = Some(crate::LoadedSkills {
        entries: Vec::new(),
        index: "# Skills\n\n- [deploy] Ship a release — /s/deploy/SKILL.md (project)\n".to_string(),
    });
    let instr = build_chat_request(&state)
        .instructions
        .expect("skill index should populate the instructions suffix");
    assert!(instr.contains("# Skills"));
    assert!(instr.contains("[deploy] Ship a release"));
}

#[test]
fn build_chat_request_includes_current_working_directory() {
    let state = fresh_state();
    let request = build_chat_request(&state);
    assert!(request.system_prompt.contains("Current Session"));
    assert!(
        request
            .system_prompt
            .contains("Current working directory: /tmp/project")
    );
    assert!(
        request
            .system_prompt
            .contains("Treat this as the project root")
    );
    // The live safety mode must be surfaced so the model knows the current
    // policy instead of inferring it from a stale tool error.
    assert!(
        request.system_prompt.contains("Safety mode: "),
        "system prompt must surface the live safety mode"
    );
}

#[test]
fn esc_during_turn_transitions_to_cancelling() {
    let mut state = fresh_state();
    state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
    let msg = Msg::Key(Key {
        code: KeyCode::Escape,
        modifiers: KeyMods::default(),
    });
    let (state, cmds) = update(state, msg);
    assert!(matches!(
        state.turn,
        TurnState::Cancelling { id: TurnId(5), .. }
    ));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::CancelScope(TurnId(5))))
    );
}

#[test]
fn esc_while_already_cancelling_does_not_exit() {
    // Esc must NEVER quit mermaid — a second Esc mid-cancel is a no-op,
    // not a force-exit. (Regression: it used to call request_exit, which
    // booted the user out and could leave a background process holding the
    // terminal. Only Ctrl+C / `/quit` exit.)
    let mut state = fresh_state();
    state.turn = TurnState::Cancelling {
        id: TurnId(5),
        since: std::time::SystemTime::now(),
    };
    let msg = Msg::Key(Key {
        code: KeyCode::Escape,
        modifiers: KeyMods::default(),
    });
    let (state, cmds) = update(state, msg);
    assert!(!state.should_exit, "Esc must not exit mermaid");
    assert!(
        !cmds.iter().any(|c| matches!(c, Cmd::Exit)),
        "Esc must not emit Cmd::Exit"
    );
    assert!(
        matches!(state.turn, TurnState::Cancelling { id: TurnId(5), .. }),
        "a second Esc mid-cancel leaves the turn cancelling, unchanged"
    );
}

#[test]
fn double_cancel_does_not_emit_twice() {
    let mut state = fresh_state();
    state.turn = TurnState::Cancelling {
        id: TurnId(1),
        since: std::time::SystemTime::now(),
    };
    let (_state, cmds) = update(state, Msg::CancelTurn);
    assert!(cmds.is_empty());
}

#[test]
fn submit_prompt_on_idle_transitions_to_generating() {
    let state = fresh_state();
    let msg = Msg::SubmitPrompt {
        text: "hi there".to_string(),
        attachment_ids: vec![],
    };
    let (state, cmds) = update(state, msg);
    assert!(matches!(state.turn, TurnState::Generating { .. }));
    // CallModel only — instructions/memory freshness comes from the config
    // watcher (#45) in the TUI and a synchronous load in the one-shot paths,
    // so submit never refreshes inline.
    assert!(cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })));
    // user message committed
    assert_eq!(state.session.messages().len(), 1);
    assert_eq!(state.session.messages()[0].content, "hi there");
}

#[test]
fn submit_prompt_when_busy_is_queued() {
    let mut state = fresh_state();
    state.turn = start_generating(TurnId(1), std::time::SystemTime::now());
    let msg = Msg::SubmitPrompt {
        text: "queue me".to_string(),
        attachment_ids: vec![],
    };
    let (state, cmds) = update(state, msg);
    assert!(matches!(
        state.turn,
        TurnState::Generating { id: TurnId(1), .. }
    ));
    assert!(cmds.is_empty());
    // Not committed to the session — but it IS queued (the old name
    // `..._is_dropped` was misleading: the message is held, not discarded).
    assert!(state.session.messages().is_empty());
    assert_eq!(state.ui.queued_messages.len(), 1);
    assert_eq!(state.ui.queued_messages[0].text, "queue me");
}

#[test]
fn queued_messages_are_capped_dropping_oldest() {
    let mut state = fresh_state();
    state.turn = start_generating(TurnId(1), std::time::SystemTime::now());
    for i in 0..(MAX_QUEUED_MESSAGES + 5) {
        let (s, _) = update(
            state,
            Msg::SubmitPrompt {
                text: format!("msg {i}"),
                attachment_ids: vec![],
            },
        );
        state = s;
    }
    assert_eq!(state.ui.queued_messages.len(), MAX_QUEUED_MESSAGES);
    // The oldest were dropped: the queue window is the last MAX_QUEUED_MESSAGES.
    assert_eq!(state.ui.queued_messages.front().unwrap().text, "msg 5");
    assert_eq!(
        state.ui.queued_messages.back().unwrap().text,
        format!("msg {}", MAX_QUEUED_MESSAGES + 4)
    );
}

#[test]
fn cancelled_turn_submits_oldest_queued_message() {
    let mut state = fresh_state();
    state.turn = TurnState::Cancelling {
        id: TurnId(1),
        since: std::time::SystemTime::now(),
    };
    state
        .ui
        .queued_messages
        .push_back(crate::state::QueuedMessage {
            text: "first queued".to_string(),
            attachment_ids: Vec::new(),
        });
    state
        .ui
        .queued_messages
        .push_back(crate::state::QueuedMessage {
            text: "second queued".to_string(),
            attachment_ids: Vec::new(),
        });

    let (state, cmds) = update(state, Msg::TurnCancelled(TurnId(1)));

    assert!(matches!(state.turn, TurnState::Generating { .. }));
    assert!(cmds.iter().any(|cmd| matches!(cmd, Cmd::CallModel { .. })));
    assert_eq!(state.session.messages()[0].content, "first queued");
    assert_eq!(state.ui.queued_messages.len(), 1);
    assert_eq!(
        state.ui.queued_messages.front().map(|q| q.text.as_str()),
        Some("second queued")
    );
}

#[test]
fn submit_prompt_trims_empty_input() {
    let state = fresh_state();
    let msg = Msg::SubmitPrompt {
        text: "   \n\t".to_string(),
        attachment_ids: vec![],
    };
    let (state, cmds) = update(state, msg);
    assert!(matches!(state.turn, TurnState::Idle));
    assert!(cmds.is_empty());
}

#[test]
fn stale_stream_text_dropped_silently() {
    let mut state = fresh_state();
    state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
    let msg = Msg::StreamText {
        turn: TurnId(4), // stale!
        chunk: "should be dropped".to_string(),
    };
    let (state, _cmds) = update(state, msg);
    if let TurnState::Generating { partial_text, .. } = &state.turn {
        assert!(partial_text.is_empty());
    } else {
        panic!("expected Generating");
    }
}

#[test]
fn current_turn_stream_text_accumulates() {
    let mut state = fresh_state();
    state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
    let (state, _) = update(
        state,
        Msg::StreamText {
            turn: TurnId(5),
            chunk: "hello ".to_string(),
        },
    );
    let (state, _) = update(
        state,
        Msg::StreamText {
            turn: TurnId(5),
            chunk: "world".to_string(),
        },
    );
    if let TurnState::Generating {
        partial_text,
        phase,
        ..
    } = &state.turn
    {
        assert_eq!(partial_text, "hello world");
        assert_eq!(*phase, GenPhase::Streaming);
    } else {
        panic!("expected Generating");
    }
}

#[test]
fn reasoning_chunk_transitions_phase_to_thinking() {
    let mut state = fresh_state();
    state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
    let (state, _) = update(
        state,
        Msg::StreamReasoning {
            turn: TurnId(5),
            chunk: mermaid_model::models::ReasoningChunk {
                text: "weighing...".to_string(),
                signature: None,
            },
        },
    );
    if let TurnState::Generating {
        phase,
        partial_reasoning,
        tokens,
        ..
    } = &state.turn
    {
        assert_eq!(*phase, GenPhase::Thinking);
        assert_eq!(partial_reasoning, "weighing...");
        // The live token counter must climb during thinking, not sit at 0
        // until answer text arrives.
        assert!(*tokens > 0, "reasoning must advance the live token counter");
    } else {
        panic!("expected Generating");
    }
}

#[test]
fn stream_done_commits_assistant_message_and_returns_to_idle() {
    let mut state = fresh_state();
    state.turn = TurnState::Generating {
        id: TurnId(5),
        started: std::time::SystemTime::now(),
        partial_text: "final answer".to_string(),
        partial_reasoning: String::new(),
        tokens: 0,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: Vec::new(),
        continuation: false,
    };
    let (state, cmds) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(5),
            usage: None,
            provider_continuation: None,
            stop_reason: None,
        },
    );
    assert!(matches!(state.turn, TurnState::Idle));
    assert_eq!(state.session.messages().len(), 1);
    assert_eq!(state.session.messages()[0].content, "final answer");
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::SaveConversation { .. }))
    );
}

#[test]
fn submit_anchors_run_and_resets_token_counter() {
    let mut state = fresh_state();
    // Stale values from a previous run must not leak into the new one.
    state.runtime.run_tokens.add_estimate(999);
    state.runtime.run_started = None;
    let (state, _) = update(
        state,
        Msg::SubmitPrompt {
            text: "hi".to_string(),
            attachment_ids: vec![],
        },
    );
    assert!(
        state.runtime.run_started.is_some(),
        "run anchor set on submit"
    );
    assert_eq!(
        state.runtime.run_tokens,
        Default::default(),
        "token counter reset on submit"
    );
}

/// Drive a natural run end (`StreamDone` with no tool calls) on a state
/// whose run anchor is set, returning the post-run state and cmds.
fn finish_run(mut state: State) -> (State, Vec<Cmd>) {
    state.runtime.run_started =
        Some(std::time::SystemTime::from(state.now) - std::time::Duration::from_secs(30));
    state.turn = TurnState::Generating {
        id: TurnId(5),
        started: std::time::SystemTime::now(),
        partial_text: "done".to_string(),
        partial_reasoning: String::new(),
        tokens: 0,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: Vec::new(),
        continuation: false,
    };
    update(
        state,
        Msg::StreamDone {
            turn: TurnId(5),
            usage: None,
            provider_continuation: None,
            stop_reason: None,
        },
    )
}

/// A fully-completed checklist's lifetime is the run's: natural run end
/// retires it (store cleared, broker synced) and the summary line absorbs
/// the count — the record of the work, where the run's totals live.
#[test]
fn run_end_retires_a_fully_completed_checklist() {
    use crate::ChecklistStatus::Completed;
    let mut state = fresh_state();
    state.session.conversation.tasks = sample_task_store(&[Completed, Completed, Completed]);
    let (state, cmds) = finish_run(state);
    assert!(
        state.session.conversation.tasks.is_empty(),
        "the finished checklist is emptied at run end"
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::SyncTaskStore(store) if store.visible().count() == 0)),
        "the broker mirror is cleared too: {cmds:?}"
    );
    let summary = state
        .session
        .messages()
        .iter()
        .find(|m| m.kind == mermaid_model::models::ChatMessageKind::RunSummary)
        .expect("run summary");
    assert!(
        summary.content.contains("3 tasks completed"),
        "the summary absorbs the retired count: {:?}",
        summary.content
    );
}

/// Unfinished work carries across runs — that is the feature. Only a
/// fully-green list retires.
#[test]
fn run_end_keeps_a_checklist_with_unfinished_work() {
    use crate::ChecklistStatus::{Completed, Pending};
    let mut state = fresh_state();
    state.session.conversation.tasks = sample_task_store(&[Completed, Pending]);
    let (state, cmds) = finish_run(state);
    assert_eq!(
        state.session.conversation.tasks.visible().count(),
        2,
        "unfinished checklist survives the run end"
    );
    assert!(
        !cmds.iter().any(|c| matches!(c, Cmd::SyncTaskStore(_))),
        "no broker churn for a surviving list"
    );
    let summary = state
        .session
        .messages()
        .iter()
        .find(|m| m.kind == mermaid_model::models::ChatMessageKind::RunSummary)
        .expect("run summary");
    assert!(
        !summary.content.contains("completed"),
        "no retirement note for a surviving list: {:?}",
        summary.content
    );
}

/// A cancelled run never reaches the natural-completion block, so its
/// checklist — green or not — stays put.
#[test]
fn cancelled_run_keeps_a_fully_completed_checklist() {
    use crate::ChecklistStatus::Completed;
    let mut state = fresh_state();
    state.session.conversation.tasks = sample_task_store(&[Completed, Completed]);
    state.runtime.run_started = Some(std::time::SystemTime::from(state.now));
    state.turn = TurnState::Cancelling {
        id: TurnId(5),
        since: std::time::SystemTime::now(),
    };
    let (state, _) = update(state, Msg::TurnCancelled(TurnId(5)));
    assert_eq!(
        state.session.conversation.tasks.visible().count(),
        2,
        "cancel is not completion; the checklist stays"
    );
    // ...but the run still ended: its summary fires (marked interrupted),
    // without the retirement note a natural completion would add.
    let summary = state
        .session
        .messages()
        .iter()
        .find(|m| m.kind == mermaid_model::models::ChatMessageKind::RunSummary)
        .expect("a cancelled run still records its summary");
    assert!(
        summary.content.contains("interrupted") && !summary.content.contains("completed"),
        "{:?}",
        summary.content
    );
}

/// Every way a run ends leaves a summary in the saved log. The natural
/// path is pinned above; these are the abnormal ends the field logs
/// showed skipping it entirely (`20260704_155044` has none at all —
/// precisely the runs whose duration and spend matter most).
#[test]
fn upstream_error_still_emits_an_interrupted_run_summary() {
    let mut state = fresh_state();
    state.runtime.run_started =
        Some(std::time::SystemTime::from(state.now) - std::time::Duration::from_secs(72));
    state.runtime.run_tokens.add_provider(1_500);
    state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
    let err = mermaid_model::models::UserFacingError {
        summary: "Server error".to_string(),
        message: "500 internal".to_string(),
        suggestion: "retry".to_string(),
        category: mermaid_model::models::ErrorCategory::Temporary,
        recoverable: true,
    };
    let (state, cmds) = update(
        state,
        Msg::UpstreamError {
            turn: TurnId(5),
            error: err,
        },
    );
    let summary = state
        .session
        .messages()
        .iter()
        .find(|m| m.kind == mermaid_model::models::ChatMessageKind::RunSummary)
        .expect("an errored run still records its summary");
    assert!(
        summary.content.contains("Worked for 1m 12s"),
        "{:?}",
        summary.content
    );
    // Provider-reported totals carry into the summary unmarked — the
    // `~` taint is reserved for estimator-filled gaps (the matched pair
    // is `run_end_appends_a_display_only_summary_once`, whose usage-less
    // final phase must show `used ~`).
    assert!(
        summary.content.contains("used 1.5k tokens") && !summary.content.contains('~'),
        "{:?}",
        summary.content
    );
    assert!(
        summary.content.contains("interrupted"),
        "{:?}",
        summary.content
    );
    assert!(
        state.runtime.run_started.is_none(),
        "the summary fires exactly once per run"
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::SaveConversation { .. }))
    );
    // Transcript order: the error line first, then the run's summary.
    assert_eq!(state.session.messages().len(), 2);
    assert_eq!(
        state.session.messages()[1].kind,
        mermaid_model::models::ChatMessageKind::RunSummary
    );
}

#[test]
fn cancelling_emits_the_summary_only_when_the_turn_actually_ends() {
    let mut state = fresh_state();
    state.runtime.run_started = Some(std::time::SystemTime::from(state.now));
    state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
    let (state, _) = update(state, Msg::CancelTurn);
    assert!(
        !state
            .session
            .messages()
            .iter()
            .any(|m| m.kind == mermaid_model::models::ChatMessageKind::RunSummary),
        "no summary while Cancelling — the run has not ended yet"
    );
    let (state, _) = update(state, Msg::TurnCancelled(TurnId(5)));
    assert!(
        state
            .session
            .messages()
            .iter()
            .any(|m| m.kind == mermaid_model::models::ChatMessageKind::RunSummary),
        "the terminal TurnCancelled ends the run and records its summary"
    );
    assert!(state.runtime.run_started.is_none());
}

#[test]
fn quitting_mid_run_records_the_summary_in_the_final_save() {
    let mut state = fresh_state();
    state.runtime.run_started = Some(std::time::SystemTime::from(state.now));
    state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
    let mut cmds = Vec::new();
    super::request_exit(&mut state, &mut cmds);
    assert!(
        state.session.messages().iter().any(|m| m.kind
            == mermaid_model::models::ChatMessageKind::RunSummary
            && m.content.contains("interrupted")),
        "quit mid-run still records the run summary"
    );
    // The final save's snapshot — what `--continue` reloads — carries it.
    let last_save = cmds
        .iter()
        .rev()
        .find_map(|c| {
            if let Cmd::SaveConversation { snapshot, .. } = c {
                Some(snapshot)
            } else {
                None
            }
        })
        .expect("exit saves the conversation");
    assert!(
        last_save
            .messages()
            .iter()
            .any(|m| m.kind == mermaid_model::models::ChatMessageKind::RunSummary),
    );
    assert!(cmds.iter().any(|c| matches!(c, Cmd::Exit)));
}

/// Matched pair for the counter reset: a run abandoned by loading another
/// conversation belongs to the OLD transcript — quitting afterwards must
/// not stamp its summary into the newly loaded one.
#[test]
fn switching_conversations_drops_the_abandoned_runs_summary() {
    let mut state = fresh_state();
    state.runtime.run_started = Some(std::time::SystemTime::from(state.now));
    state.runtime.run_tokens.add_provider(500);
    state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
    let history = fresh_state().session.conversation;
    let (mut state, _) = update(
        state,
        Msg::QueryResult(QueryResult::ConversationLoaded(Box::new(history))),
    );
    let mut cmds = Vec::new();
    super::request_exit(&mut state, &mut cmds);
    assert!(
        !state
            .session
            .messages()
            .iter()
            .any(|m| m.kind == mermaid_model::models::ChatMessageKind::RunSummary),
        "the abandoned run's summary must not land in the loaded conversation"
    );
}

/// Resume normalization: a save carrying an all-done checklist (written
/// before run-end retirement existed, or killed at the wrong moment)
/// loads with an empty store instead of resurrecting a zombie band.
#[test]
fn seeding_a_conversation_preserves_the_saved_checklist() {
    use crate::ChecklistStatus::{Completed, Pending};
    let mut history = crate::ConversationHistory::new(
        "/tmp/project".to_string(),
        "ollama/test".to_string(),
        chrono::Local::now(),
    );
    history.tasks = sample_task_store(&[Completed, Completed]);
    let mut state = fresh_state();
    state.seed_conversation(history);
    // Retirement happens at natural run end and NOWHERE else. A saved
    // all-done list belongs to a run that did not end naturally (cancelled,
    // errored, or killed), and run end deliberately preserves those — so
    // clearing it here was a second, contradictory rule that silently
    // discarded the user's checklist on resume.
    assert_eq!(
        state.session.conversation.tasks.visible().count(),
        2,
        "a saved checklist survives resume; only run end retires one",
    );
    // And an unfinished list still resumes intact.
    let mut history = crate::ConversationHistory::new(
        "/tmp/project".to_string(),
        "ollama/test".to_string(),
        chrono::Local::now(),
    );
    history.tasks = sample_task_store(&[Completed, Pending]);
    let mut state = fresh_state();
    state.seed_conversation(history);
    assert_eq!(state.session.conversation.tasks.visible().count(), 2);
}

#[test]
fn run_end_appends_a_display_only_summary_once() {
    let mut state = fresh_state();
    // Run started 72s ago with some generated tokens.
    state.runtime.run_started =
        Some(std::time::SystemTime::from(state.now) - std::time::Duration::from_secs(72));
    state.runtime.run_tokens.add_provider(1500);
    state.turn = TurnState::Generating {
        id: TurnId(5),
        started: std::time::SystemTime::now(),
        partial_text: "final answer".to_string(),
        partial_reasoning: String::new(),
        tokens: 0,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: Vec::new(),
        continuation: false,
    };
    let (state, _) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(5),
            usage: None,
            provider_continuation: None,
            stop_reason: None,
        },
    );
    let summary = state
        .session
        .messages()
        .iter()
        .find(|m| m.kind == mermaid_model::models::ChatMessageKind::RunSummary)
        .expect("a run summary should be appended at run end");
    assert!(summary.content.contains("Worked for"));
    assert!(
        summary.content.contains("1m 12s"),
        "72s should format as 1m 12s, got {:?}",
        summary.content
    );
    assert!(
        state.runtime.run_started.is_none(),
        "run_started is cleared so the summary fires exactly once per run"
    );
    assert!(
        summary.content.contains("used ~"),
        "a usage-less final phase falls back to a chars/4 estimate and \
             must mark the total `~`, got {:?}",
        summary.content
    );
    assert!(
        !summary.content.contains("interrupted"),
        "a naturally-completed run is not interrupted: {:?}",
        summary.content
    );
}

#[test]
fn run_summary_shows_line_change_totals() {
    let mut state = fresh_state();
    state.runtime.run_started =
        Some(std::time::SystemTime::from(state.now) - std::time::Duration::from_secs(10));
    state.runtime.run_line_changes.add(4, 4);
    state.turn = TurnState::Generating {
        id: TurnId(5),
        started: std::time::SystemTime::now(),
        partial_text: "final answer".to_string(),
        partial_reasoning: String::new(),
        tokens: 0,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: Vec::new(),
        continuation: false,
    };
    let (state, _) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(5),
            usage: None,
            provider_continuation: None,
            stop_reason: None,
        },
    );
    let summary = state
        .session
        .messages()
        .iter()
        .find(|m| m.kind == mermaid_model::models::ChatMessageKind::RunSummary)
        .expect("a run summary should be appended at run end");
    assert!(
        summary.content.contains("· +4/-4"),
        "a run that mutated files totals its line changes, got {:?}",
        summary.content
    );
}

#[test]
fn run_summary_omits_line_changes_when_nothing_changed() {
    let mut state = fresh_state();
    state.runtime.run_started =
        Some(std::time::SystemTime::from(state.now) - std::time::Duration::from_secs(10));
    state.turn = TurnState::Generating {
        id: TurnId(5),
        started: std::time::SystemTime::now(),
        partial_text: "final answer".to_string(),
        partial_reasoning: String::new(),
        tokens: 0,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: Vec::new(),
        continuation: false,
    };
    let (state, _) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(5),
            usage: None,
            provider_continuation: None,
            stop_reason: None,
        },
    );
    let summary = state
        .session
        .messages()
        .iter()
        .find(|m| m.kind == mermaid_model::models::ChatMessageKind::RunSummary)
        .expect("a run summary should be appended at run end");
    assert!(
        !summary.content.contains('+'),
        "a read-only run keeps the two-part summary, got {:?}",
        summary.content
    );
}

#[test]
fn tool_finished_folds_line_changes_into_run_totals() {
    let mut state = fresh_state();
    state
        .session
        .append(ChatMessage::assistant("editing a file"), state.now);
    state.turn = crate::transition::start_executing_tools(
        TurnId(1),
        vec![pending_read_file_call()],
        std::time::SystemTime::now(),
    );
    let outcome = ToolOutcome::success("Wrote foo.rs (3 lines)", "3 lines written", 0.1)
        .with_metadata(mermaid_model::tool_run::ToolRunMetadata {
            lines_added: 3,
            lines_removed: 1,
            ..Default::default()
        });
    let (state, _) = update(
        state,
        Msg::ToolFinished {
            turn: TurnId(1),
            call_id: crate::ToolCallId(1),
            outcome,
        },
    );
    assert_eq!(
        state.runtime.run_line_changes,
        crate::runtime::RunLineChanges {
            added: 3,
            removed: 1
        },
        "exact metadata counts accumulate across the run"
    );
}

#[test]
fn run_summary_uses_real_provider_output_unmarked() {
    let mut state = fresh_state();
    state.runtime.run_started =
        Some(std::time::SystemTime::from(state.now) - std::time::Duration::from_secs(10));
    state.turn = TurnState::Generating {
        id: TurnId(5),
        started: std::time::SystemTime::now(),
        partial_text: "final answer".to_string(),
        partial_reasoning: String::new(),
        tokens: 0,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: Vec::new(),
        continuation: false,
    };
    let (state, _) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(5),
            usage: Some(
                mermaid_model::models::TokenUsage::provider(1_000, 30).with_reasoning_output(20),
            ),
            provider_continuation: None,
            stop_reason: None,
        },
    );
    let summary = state
        .session
        .messages()
        .iter()
        .find(|m| m.kind == mermaid_model::models::ChatMessageKind::RunSummary)
        .expect("a run summary should be appended at run end");
    assert!(
        summary.content.contains("used 50 tokens"),
        "provider-reported output (completion 30 + reasoning 20) with no \
             `~`, got {:?}",
        summary.content
    );
}

#[test]
fn build_chat_request_excludes_run_summaries() {
    let mut state = fresh_state();
    state.session.append(ChatMessage::user("hello"), state.now);
    state.session.append(
        ChatMessage::run_summary("Worked for 5s · used 100 tokens"),
        state.now,
    );
    let req = build_chat_request(&state);
    assert!(
        !req.messages
            .iter()
            .any(|m| m.kind == mermaid_model::models::ChatMessageKind::RunSummary),
        "run summaries are display-only and must not reach the model"
    );
    assert!(
        req.messages.iter().any(|m| m.content == "hello"),
        "real conversation messages are still sent"
    );
}

#[test]
fn run_token_counter_banks_each_phase_across_tool_steps() {
    // The counter must accumulate, not reset, as each model call completes —
    // so a multi-step agentic run shows one growing total.
    let mut state = fresh_state();
    state.runtime.run_tokens.add_provider(100); // earlier phases this run
    state.turn = TurnState::Generating {
        id: TurnId(5),
        started: std::time::SystemTime::now(),
        partial_text: "x".repeat(400),      // ~100 tokens
        partial_reasoning: "y".repeat(400), // ~100 tokens
        tokens: 200,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: Vec::new(),
        continuation: false,
    };
    let (state, _) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(5),
            usage: None,
            provider_continuation: None,
            stop_reason: None,
        },
    );
    // 100 prior + (400 + 400)/4 = 200 this phase — an estimate, because
    // this Done carried no provider usage, so the counter is tainted `~`.
    assert_eq!(state.runtime.run_tokens.output_tokens, 300);
    assert!(state.runtime.run_tokens.contains_estimate);
}

#[test]
fn stream_done_completely_empty_turn_auto_retries() {
    // No text, no reasoning, no tool calls → previously a silent dead-end (or a
    // bare hint). Now it auto-retries the model call (bounded), same as a
    // reasoning-heavy stall — the "no visible output" test doesn't hinge on
    // whether the model happened to emit hidden reasoning.
    let mut state = fresh_state();
    state.turn = TurnState::Generating {
        id: TurnId(5),
        started: std::time::SystemTime::now(),
        partial_text: String::new(),
        partial_reasoning: String::new(),
        tokens: 0,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: Vec::new(),
        continuation: false,
    };
    let (state, cmds) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(5),
            usage: None,
            provider_continuation: None,
            stop_reason: None,
        },
    );
    assert!(
        cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })),
        "a completely empty turn must re-issue the model call, not dead-end"
    );
    assert!(matches!(state.turn, TurnState::Generating { .. }));
    assert_eq!(state.runtime.empty_continuations, 1);
}

#[test]
fn stream_done_does_not_flag_reasoning_only_turn() {
    // Reasoning-only (hidden) is not "empty" — it renders as "Reasoning
    // hidden", so the empty-output note must NOT fire.
    let mut state = fresh_state();
    state.turn = TurnState::Generating {
        id: TurnId(5),
        started: std::time::SystemTime::now(),
        partial_text: String::new(),
        partial_reasoning: "thinking it through".to_string(),
        tokens: 0,
        phase: GenPhase::Thinking,
        provider_continuation: None,
        pending_tool_calls: Vec::new(),
        continuation: false,
    };
    let (state, _) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(5),
            usage: None,
            provider_continuation: None,
            stop_reason: None,
        },
    );
    assert!(
        !state
            .session
            .messages()
            .iter()
            .any(|m| m.content.contains("no output")),
        "reasoning-only turn is not empty"
    );
}

// ── Length-truncation recovery (compact + continue) ──────────────────

fn truncating_turn(partial: &str) -> TurnState {
    TurnState::Generating {
        id: TurnId(5),
        started: std::time::SystemTime::now(),
        partial_text: partial.to_string(),
        partial_reasoning: String::new(),
        tokens: 0,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: Vec::new(),
        continuation: false,
    }
}

fn length_done() -> Msg {
    Msg::StreamDone {
        turn: TurnId(5),
        usage: None,
        provider_continuation: None,
        stop_reason: Some(mermaid_model::models::FinishReason::Length),
    }
}

#[test]
fn length_truncation_recovers_by_compacting_and_continuing() {
    let mut state = fresh_state();
    state
        .session
        .append(ChatMessage::user("build a site"), state.now);
    state
        .session
        .append(ChatMessage::assistant("ok, writing files"), state.now);
    state.turn = truncating_turn("let me fix the");
    let (state, cmds) = update(state, length_done());

    assert!(
        matches!(
            state.turn,
            TurnState::Compacting {
                trigger: CompactionTrigger::TruncationRecovery,
                ..
            }
        ),
        "a recoverable truncation compacts instead of ending the run"
    );
    assert_eq!(state.runtime.truncation_recoveries, 1, "recovery counted");
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::CompactConversation { request, .. }
                if request.trigger == CompactionTrigger::TruncationRecovery)),
        "emits a truncation-recovery compaction"
    );
    assert!(
        state.session.messages().iter().any(|m| m
            .content
            .contains("compacting the conversation to continue")),
        "tells the user it's recovering"
    );
    assert!(
        !state
            .session
            .messages()
            .iter()
            .any(|m| m.content.contains("Response truncated")),
        "no terminal hint while recovering"
    );
}

#[test]
fn length_truncation_without_progress_at_cap_stops_with_hint() {
    let mut state = fresh_state();
    state
        .session
        .append(ChatMessage::user("build a site"), state.now);
    state
        .session
        .append(ChatMessage::assistant("ok"), state.now);
    // Already at the default cap of consecutive recoveries.
    state.runtime.truncation_recoveries =
        state.settings.compaction.max_truncation_recoveries as u32;
    state.turn = truncating_turn("");
    let (state, cmds) = update(state, length_done());

    assert!(matches!(state.turn, TurnState::Idle), "run ends at the cap");
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::CompactConversation { .. })),
        "no further compaction once capped"
    );
    assert!(
        state
            .session
            .messages()
            .iter()
            .any(|m| m.content.contains("Response truncated")),
        "shows the manual-levers hint at the cap"
    );
}

#[test]
fn length_truncation_uncapped_keeps_recovering() {
    let mut state = fresh_state();
    state.session.append(ChatMessage::user("x"), state.now);
    state.session.append(ChatMessage::assistant("y"), state.now);
    state.settings.compaction.max_truncation_recoveries = 0; // uncapped
    state.runtime.truncation_recoveries = 99; // would exceed any finite cap
    state.turn = truncating_turn("z");
    let (state, cmds) = update(state, length_done());

    assert!(
        matches!(
            state.turn,
            TurnState::Compacting {
                trigger: CompactionTrigger::TruncationRecovery,
                ..
            }
        ),
        "cap 0 means recover regardless of the count"
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::CompactConversation { .. }))
    );
}

#[test]
fn length_truncation_without_history_stops_with_hint() {
    // Only the truncated message exists — nothing to compact, so just inform.
    let mut state = fresh_state();
    state.turn = truncating_turn("partial");
    let (state, cmds) = update(state, length_done());

    assert!(matches!(state.turn, TurnState::Idle));
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::CompactConversation { .. }))
    );
    assert!(
        state
            .session
            .messages()
            .iter()
            .any(|m| m.content.contains("Response truncated"))
    );
}

#[test]
fn truncation_recoveries_reset_when_run_makes_progress() {
    // A normal (non-truncated) completion is progress and clears the guard, so
    // the cap counts only *consecutive* no-progress truncations.
    let mut state = fresh_state();
    state.runtime.truncation_recoveries = 2;
    state.turn = truncating_turn("a clean final answer");
    let (state, _) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(5),
            usage: None,
            provider_continuation: None,
            stop_reason: None, // not a length truncation
        },
    );
    assert_eq!(state.runtime.truncation_recoveries, 0);
}

fn length_done_with_usage(prompt: usize, completion: usize) -> Msg {
    Msg::StreamDone {
        turn: TurnId(5),
        usage: Some(mermaid_model::models::TokenUsage::provider(
            prompt, completion,
        )),
        provider_continuation: None,
        stop_reason: Some(mermaid_model::models::FinishReason::Length),
    }
}

#[test]
fn length_output_cap_continues_in_fresh_turn_not_compaction() {
    // The GLM-5.2 case: a length-stop with usage present and the window
    // unknown (the normal remote-provider state) is the per-response
    // OUTPUT cap — compacting the input can't help. With visible content
    // committed, the run now CONTINUES the reply in a fresh turn.
    let mut state = fresh_state();
    state
        .session
        .append(ChatMessage::user("audit the widget"), state.now);
    state
        .session
        .append(ChatMessage::assistant("exploring the code"), state.now);
    state.turn = truncating_turn("here is the audit so f");
    let (state, cmds) = update(state, length_done_with_usage(16_600, 4_000));

    assert!(
        matches!(state.turn, TurnState::Generating { .. }),
        "output-cap with content continues in a fresh turn"
    );
    assert!(
        cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })),
        "the continuation model call is dispatched"
    );
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::CompactConversation { .. })),
        "output-cap truncation must never dispatch a compaction"
    );
    assert_eq!(state.runtime.continue_recoveries, 1);
    assert!(
        state
            .session
            .messages()
            .iter()
            .any(|m| m.content.contains("continuing")),
        "the continuation note rides in history to nudge the model"
    );
    assert!(
        !state
            .session
            .messages()
            .iter()
            .any(|m| m.content.contains("Context window full")),
        "the misdiagnosed window-full message is gone"
    );
}

#[test]
fn length_output_cap_mid_reasoning_stops_with_hint() {
    // Cut off mid-think (nearly all completion tokens were reasoning): a
    // continuation can't stitch a hidden trace, so stop with the accurate
    // hint instead.
    let mut state = fresh_state();
    state
        .session
        .append(ChatMessage::user("audit the widget"), state.now);
    state
        .session
        .append(ChatMessage::assistant("exploring the code"), state.now);
    state.turn = truncating_turn("here is");
    let usage =
        mermaid_model::models::TokenUsage::provider(16_600, 4_000).with_reasoning_output(3_900);
    let (state, cmds) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(5),
            usage: Some(usage),
            provider_continuation: None,
            stop_reason: Some(mermaid_model::models::FinishReason::Length),
        },
    );

    assert!(matches!(state.turn, TurnState::Idle), "no continuation");
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })));
    assert_eq!(state.runtime.continue_recoveries, 0);
    assert!(
        state
            .session
            .messages()
            .iter()
            .any(|m| m.content.contains("per-response output limit")),
        "the accurate output-cap hint is shown"
    );
}

#[test]
fn length_output_cap_respects_continuation_cap() {
    // At the per-run continuation cap the run stops with the hint instead
    // of looping forever on a model that keeps re-truncating.
    let mut state = fresh_state();
    state.runtime.continue_recoveries = mermaid_model::constants::MAX_OUTPUT_CONTINUATIONS;
    state
        .session
        .append(ChatMessage::user("audit the widget"), state.now);
    state
        .session
        .append(ChatMessage::assistant("exploring the code"), state.now);
    state.turn = truncating_turn("here is the audit so f");
    let (state, cmds) = update(state, length_done_with_usage(16_600, 4_000));

    assert!(matches!(state.turn, TurnState::Idle));
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })));
    assert!(
        state
            .session
            .messages()
            .iter()
            .any(|m| m.content.contains("per-response output limit")),
    );
}

#[test]
fn continue_recoveries_reset_when_run_makes_progress() {
    // Any non-truncation ending is progress — the continuation guard only
    // counts consecutive output-cap truncations.
    let mut state = fresh_state();
    state.runtime.continue_recoveries = 3;
    state.turn = truncating_turn("a clean final answer");
    let (state, _) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(5),
            usage: None,
            provider_continuation: None,
            stop_reason: None,
        },
    );
    assert_eq!(state.runtime.continue_recoveries, 0);
}

/// A live continuation turn (mid auto-continue) with accumulated text.
fn continuation_turn(id: TurnId, partial: &str) -> TurnState {
    TurnState::Generating {
        id,
        started: std::time::SystemTime::now(),
        partial_text: partial.to_string(),
        partial_reasoning: String::new(),
        tokens: 0,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: Vec::new(),
        continuation: true,
    }
}

#[test]
fn continuation_full_cycle_stamps_kind_and_sweeps_spent_nudge() {
    // The whole seamless-stitch contract at the domain layer: the nudge is
    // stamped RecoveryNudge (hidden + retirable), the continuation turn
    // carries the flag, its commit is stamped Continuation, and the spent
    // nudge is swept at the continuation's stream-done — leaving partial
    // and continuation ADJACENT in history so the transcript can merge
    // them. An unrelated system note must survive the sweep.
    let mut state = fresh_state();
    state
        .session
        .append(ChatMessage::user("audit the widget"), state.now);
    state
        .session
        .append(ChatMessage::system("unrelated note"), state.now);
    state.turn = truncating_turn("part one of the reply");
    let (mut state, _) = update(state, length_done_with_usage(16_600, 4_000));

    let nudge = state
        .session
        .messages()
        .iter()
        .find(|m| m.content.contains("continuing"))
        .expect("nudge pushed");
    assert_eq!(
        nudge.kind,
        mermaid_model::models::ChatMessageKind::RecoveryNudge,
        "the nudge is stamped for hiding + retirement"
    );
    let cont_id = state.turn.id().expect("continuation turn live");
    assert!(
        matches!(
            state.turn,
            TurnState::Generating {
                continuation: true,
                ..
            }
        ),
        "the fresh turn carries the continuation flag"
    );

    // The continuation streams its half and finishes normally.
    state.turn = continuation_turn(cont_id, "and part two");
    let (state, _) = update(
        state,
        Msg::StreamDone {
            turn: cont_id,
            usage: None,
            provider_continuation: None,
            stop_reason: None,
        },
    );

    let messages = state.session.messages();
    assert!(
        !messages
            .iter()
            .any(|m| m.kind == mermaid_model::models::ChatMessageKind::RecoveryNudge),
        "the spent nudge is retired from history"
    );
    assert!(
        messages.iter().any(|m| m.content == "unrelated note"),
        "the sweep only removes recovery nudges"
    );
    let part_one = messages
        .iter()
        .position(|m| m.content == "part one of the reply")
        .expect("partial committed");
    let part_two = &messages[part_one + 1];
    assert_eq!(
        part_two.content, "and part two",
        "partial and continuation sit adjacent once the nudge is gone"
    );
    assert_eq!(
        part_two.kind,
        mermaid_model::models::ChatMessageKind::Continuation,
        "the continuation commit is stamped for the display stitch"
    );
    assert_eq!(state.runtime.continue_recoveries, 0, "progress resets");
}

#[test]
fn empty_retry_inside_continuation_chain_keeps_the_flag() {
    // A continuation turn that comes back empty auto-retries; the retry
    // turn must still be a continuation or the eventual real text commits
    // unstamped and the transcript shows a seam mid-chain.
    let mut state = fresh_state();
    state
        .session
        .append(ChatMessage::user("audit the widget"), state.now);
    state.turn = continuation_turn(TurnId(5), "");
    let (state, _) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(5),
            usage: None,
            provider_continuation: None,
            stop_reason: None,
        },
    );
    assert!(
        matches!(
            state.turn,
            TurnState::Generating {
                continuation: true,
                ..
            }
        ),
        "the empty-retry turn inherits the continuation flag"
    );
    let nudge = state
        .session
        .messages()
        .iter()
        .find(|m| m.content.contains("no reply or action"))
        .expect("empty-retry nudge pushed");
    assert_eq!(
        nudge.kind,
        mermaid_model::models::ChatMessageKind::RecoveryNudge,
        "the stalled-turn nudge gets the same retirement treatment"
    );
}

#[test]
fn truncation_recovery_resume_keeps_continuation_flag() {
    // A continuation turn that hits a genuine context-full stop compacts;
    // the resume after compaction must re-enter Generating with the flag.
    let mut state = fresh_state();
    state
        .session
        .append(ChatMessage::user("original prompt"), state.now);
    state.turn = TurnState::Compacting {
        id: TurnId(7),
        started: std::time::SystemTime::now(),
        trigger: CompactionTrigger::TruncationRecovery,
        resume_continuation: true,
    };
    let result = fake_recovery_result(vec![ChatMessage::user("compacted context")]);
    let (state, _) = update(
        state,
        Msg::CompactionFinished {
            turn: TurnId(7),
            result,
        },
    );
    assert!(
        matches!(
            state.turn,
            TurnState::Generating {
                continuation: true,
                ..
            }
        ),
        "the compaction resume carries the chain marker through"
    );
}

#[test]
fn quit_mid_continuation_stamps_interrupted_commit_and_sweeps() {
    // Ctrl+C mid-continuation: the interrupted partial keeps the
    // Continuation stamp (a `--continue` reload still stitches) and the
    // live nudge doesn't get persisted into the saved session.
    let mut state = fresh_state();
    state
        .session
        .append(ChatMessage::user("audit the widget"), state.now);
    state
        .session
        .append(ChatMessage::assistant("part one of the reply"), state.now);
    let mut nudge = ChatMessage::system("resume nudge");
    nudge.kind = mermaid_model::models::ChatMessageKind::RecoveryNudge;
    state.session.append(nudge, state.now);
    state.turn = continuation_turn(TurnId(9), "and part tw");
    let (state, cmds) = update(state, Msg::Quit);

    assert!(state.should_exit);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::SaveConversation { .. }))
    );
    let messages = state.session.messages();
    assert!(
        !messages
            .iter()
            .any(|m| m.kind == mermaid_model::models::ChatMessageKind::RecoveryNudge),
        "the live nudge is not persisted on quit"
    );
    let last = messages.last().expect("interrupted partial committed");
    assert!(last.content.contains("and part tw"));
    assert_eq!(
        last.kind,
        mermaid_model::models::ChatMessageKind::Continuation,
        "the interrupted commit keeps the stitch marker"
    );
}

#[test]
fn cancel_and_upstream_error_sweep_spent_nudges() {
    // Both non-stream-done turn ends retire a live nudge: leaving it in
    // history would keep steering later requests while the transcript
    // hides it.
    for is_error in [false, true] {
        let mut state = fresh_state();
        let mut nudge = ChatMessage::system("resume nudge");
        nudge.kind = mermaid_model::models::ChatMessageKind::RecoveryNudge;
        state.session.append(nudge, state.now);
        let msg = if is_error {
            state.turn = continuation_turn(TurnId(9), "half");
            Msg::UpstreamError {
                turn: TurnId(9),
                error: mermaid_model::models::UserFacingError {
                    summary: "boom".to_string(),
                    message: "provider died".to_string(),
                    suggestion: String::new(),
                    category: mermaid_model::models::ErrorCategory::Temporary,
                    recoverable: true,
                },
            }
        } else {
            state.turn = TurnState::Cancelling {
                id: TurnId(9),
                since: std::time::SystemTime::now(),
            };
            Msg::TurnCancelled(TurnId(9))
        };
        let (state, cmds) = update(state, msg);
        assert!(
            !state
                .session
                .messages()
                .iter()
                .any(|m| m.kind == mermaid_model::models::ChatMessageKind::RecoveryNudge),
            "nudge swept (is_error={is_error})"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::SaveConversation { .. })),
            "sweep persisted (is_error={is_error})"
        );
    }
}

#[test]
fn submit_resets_continue_recoveries() {
    // A run that ENDED at the continuation cap never hits the in-stream
    // reset; the next submit must restore the full budget.
    let mut state = fresh_state();
    state.runtime.continue_recoveries = mermaid_model::constants::MAX_OUTPUT_CONTINUATIONS;
    let (state, _) = update(
        state,
        Msg::SubmitPrompt {
            text: "next task".to_string(),
            attachment_ids: Vec::new(),
        },
    );
    assert_eq!(state.runtime.continue_recoveries, 0);
}

#[test]
fn length_with_usage_near_known_window_still_compacts() {
    // With usage AND a known window that prompt+completion+reserve reaches,
    // the window genuinely filled — the legacy compact-and-continue
    // recovery is still correct.
    let mut state = fresh_state();
    state.runtime.provider_capabilities.max_context_tokens = Some(20_000);
    state
        .session
        .append(ChatMessage::user("build a site"), state.now);
    state
        .session
        .append(ChatMessage::assistant("ok, writing files"), state.now);
    state.turn = truncating_turn("let me fix the");
    let (state, cmds) = update(state, length_done_with_usage(18_000, 1_500));

    assert!(
        matches!(
            state.turn,
            TurnState::Compacting {
                trigger: CompactionTrigger::TruncationRecovery,
                ..
            }
        ),
        "a genuinely full window still recovers via compaction"
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::CompactConversation { .. })),
        "compaction dispatched"
    );
}

fn fake_recovery_result(replacement: Vec<ChatMessage>) -> CompactionResult {
    let snap = crate::state::ContextUsageSnapshot::from_estimate(
        crate::state::PromptTokenBreakdown::default(),
        Some(12_000),
    );
    CompactionResult {
        record: crate::CompactionEvent {
            id: "rec1".to_string(),
            trigger: CompactionTrigger::TruncationRecovery,
            created_at: chrono::Local::now(),
            before_tokens: 100,
            after_tokens: 40,
            archived_message_count: 2,
            preserved_message_count: replacement.len(),
            preserved_turn_count: replacement
                .iter()
                .filter(|message| message.role == MessageRole::User)
                .count(),
            summary_tokens: 10,
            duration_secs: 0.0,
            review_status: crate::CompactionReviewStatus::Reviewed,
            review_error: None,
            focus: None,
            archive_path: None,
        },
        replacement_messages: replacement,
        archived_messages: vec![ChatMessage::user("archived")],
        before_snapshot: snap.clone(),
        after_snapshot: snap,
        usage: None,
        source_boundaries: Vec::new(),
    }
}

#[test]
fn compaction_finish_preserves_messages_appended_after_dispatch() {
    let mut state = fresh_state();
    state.session.append(ChatMessage::user("old"), state.now);
    state
        .session
        .append(ChatMessage::assistant("old answer"), state.now);
    state.session.append(ChatMessage::user("latest"), state.now);
    let boundaries = state
        .session
        .messages()
        .iter()
        .map(crate::CompactionBoundary::from_message)
        .collect();
    state.turn = TurnState::Compacting {
        id: TurnId(7),
        started: std::time::SystemTime::now(),
        trigger: CompactionTrigger::Manual,
        resume_continuation: false,
    };
    state.session.append(
        ChatMessage::system("MCP server failed during compaction"),
        state.now,
    );

    let mut result = fake_recovery_result(vec![ChatMessage::user("compacted context")]);
    result.source_boundaries = boundaries;
    let (state, _) = update(
        state,
        Msg::CompactionFinished {
            turn: TurnId(7),
            result,
        },
    );
    assert!(
        state
            .session
            .messages()
            .iter()
            .any(|message| { message.content == "MCP server failed during compaction" })
    );
}

#[test]
fn compaction_finish_places_late_tool_results_after_the_pending_tail() {
    let mut state = fresh_state();
    state.session.append(ChatMessage::user("old"), state.now);
    state
        .session
        .append(ChatMessage::assistant("old answer"), state.now);
    state.session.append(ChatMessage::user("latest"), state.now);
    let boundaries = state
        .session
        .messages()
        .iter()
        .map(crate::CompactionBoundary::from_message)
        .collect();
    state.turn = TurnState::Compacting {
        id: TurnId(7),
        started: std::time::SystemTime::now(),
        trigger: CompactionTrigger::TruncationRecovery,
        resume_continuation: false,
    };
    // All three arrive while compaction runs: a notice, a tool result that
    // answers the pending assistant tail the checkpoint preserves, and a
    // report that (chronologically and textually) follows the result.
    state
        .session
        .append(ChatMessage::system("notice during compaction"), state.now);
    state.session.append(
        ChatMessage::tool("call_9", "execute_command", "late result"),
        state.now,
    );
    state
        .session
        .append(ChatMessage::system("report about the result"), state.now);

    let mut pending_tail = ChatMessage::assistant("");
    pending_tail.tool_calls = Some(vec![tool_call_fixture("call_9", "execute_command")]);
    let mut result =
        fake_recovery_result(vec![ChatMessage::user("compacted context"), pending_tail]);
    result.source_boundaries = boundaries;
    let (state, _) = update(
        state,
        Msg::CompactionFinished {
            turn: TurnId(7),
            result,
        },
    );

    let contents: Vec<&str> = state
        .session
        .messages()
        .iter()
        .map(|message| message.content.as_str())
        .collect();
    let tail_call = contents
        .iter()
        .position(|content| content.is_empty())
        .expect("pending tail kept");
    let tail_result = contents
        .iter()
        .position(|content| *content == "late result")
        .expect("late tool result kept");
    let notice = contents
        .iter()
        .position(|content| *content == "notice during compaction")
        .expect("notice kept");
    let report = contents
        .iter()
        .position(|content| *content == "report about the result")
        .expect("report kept");
    assert!(
        notice < tail_call,
        "pre-result intervening messages go before the pending tail"
    );
    assert!(
        tail_call < tail_result,
        "a tool result must follow its tool call"
    );
    assert!(
        tail_result < report,
        "a message that arrived after the result must stay after it"
    );
}

fn tool_call_fixture(id: &str, name: &str) -> mermaid_model::models::tool_call::ToolCall {
    mermaid_model::models::tool_call::ToolCall {
        id: Some(id.to_string()),
        function: mermaid_model::models::tool_call::FunctionCall {
            name: name.to_string(),
            arguments: serde_json::json!({}),
        },
    }
}

#[test]
fn auto_compaction_failure_pauses_retries_until_reset() {
    let mut state = fresh_state();
    state.turn = truncating_turn("");
    let turn = state.turn.id().expect("generating turn");

    let (mut state, _) = update(
        state,
        Msg::CompactionFailed {
            turn,
            trigger: CompactionTrigger::AutoThreshold,
            message: "checkpoint invalid".to_string(),
            kind: StatusKind::Warn,
        },
    );
    assert!(state.runtime.auto_compact_suppressed);
    assert!(build_chat_request(&state).suppress_auto_compact);
    let notices = |state: &State| {
        state
            .session
            .messages()
            .iter()
            .filter(|message| message.content.contains("Auto-compaction paused"))
            .count()
    };
    assert_eq!(notices(&state), 1);

    // A second failure stays silent — the pause was already announced.
    state.turn = truncating_turn("");
    let turn = state.turn.id().expect("generating turn");
    let (state, _) = update(
        state,
        Msg::CompactionFailed {
            turn,
            trigger: CompactionTrigger::AutoThreshold,
            message: "checkpoint invalid".to_string(),
            kind: StatusKind::Warn,
        },
    );
    assert!(state.runtime.auto_compact_suppressed);
    assert_eq!(notices(&state), 1);

    // A successful compaction (any trigger) lifts the pause.
    let mut state = state;
    state.turn = TurnState::Compacting {
        id: TurnId(7),
        started: std::time::SystemTime::now(),
        trigger: CompactionTrigger::Manual,
        resume_continuation: false,
    };
    let result = fake_recovery_result(vec![ChatMessage::user("compacted context")]);
    let (state, _) = update(
        state,
        Msg::CompactionFinished {
            turn: TurnId(7),
            result,
        },
    );
    assert!(!state.runtime.auto_compact_suppressed);
    assert!(!build_chat_request(&state).suppress_auto_compact);
}

#[test]
fn manual_compact_lifts_the_auto_compaction_pause() {
    let mut state = fresh_state();
    state.runtime.auto_compact_suppressed = true;
    state.session.append(ChatMessage::user("one"), state.now);
    state
        .session
        .append(ChatMessage::assistant("two"), state.now);
    state.session.append(ChatMessage::user("three"), state.now);

    let (state, cmds) = update(state, Msg::Slash(SlashCmd::Compact(None)));
    assert!(!state.runtime.auto_compact_suppressed);
    let Some(Cmd::CompactConversation { request, .. }) = cmds
        .iter()
        .find(|command| matches!(command, Cmd::CompactConversation { .. }))
    else {
        panic!("expected a CompactConversation command");
    };
    assert!(!request.chat.suppress_auto_compact);
}

#[test]
fn model_switch_lifts_the_auto_compaction_pause() {
    let mut state = fresh_state();
    state.runtime.auto_compact_suppressed = true;

    // The pause is model-scoped: switching models is the natural reaction
    // to a summarizer that can't produce the checkpoint structure, and the
    // new model deserves a fresh shot.
    let (state, _) = update(
        state,
        Msg::Slash(SlashCmd::Model(Some("ollama/other".to_string()))),
    );
    assert!(!state.runtime.auto_compact_suppressed);
}

#[test]
fn visible_context_full_progress_resets_recovery_guard() {
    let mut state = fresh_state();
    state.runtime.provider_capabilities.max_context_tokens = Some(20_000);
    state.settings.compaction.max_truncation_recoveries = 3;
    state.runtime.truncation_recoveries = 2;
    state.session.append(ChatMessage::user("one"), state.now);
    state
        .session
        .append(ChatMessage::assistant("two"), state.now);
    state.session.append(ChatMessage::user("three"), state.now);
    state.turn = truncating_turn("visible progress");

    let (state, cmds) = update(state, length_done_with_usage(18_000, 1_500));
    assert!(matches!(state.turn, TurnState::Compacting { .. }));
    assert_eq!(state.runtime.truncation_recoveries, 1);
    assert!(
        cmds.iter()
            .any(|command| matches!(command, Cmd::CompactConversation { .. }))
    );
}

#[test]
fn finished_truncation_recovery_resumes_the_run() {
    let mut state = fresh_state();
    state
        .session
        .append(ChatMessage::user("original prompt"), state.now);
    state.turn = TurnState::Compacting {
        id: TurnId(7),
        started: std::time::SystemTime::now(),
        trigger: CompactionTrigger::TruncationRecovery,
        resume_continuation: false,
    };
    let result = fake_recovery_result(vec![ChatMessage::user("compacted context")]);
    let (state, cmds) = update(
        state,
        Msg::CompactionFinished {
            turn: TurnId(7),
            result,
        },
    );

    assert!(
        matches!(state.turn, TurnState::Generating { .. }),
        "recovery resumes generating with the compacted context"
    );
    assert!(
        cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })),
        "re-dispatches the model call to finish the work"
    );
}

#[test]
fn finished_context_limit_retry_resumes_the_run() {
    // D6: a context-limit compaction must resume the interrupted request,
    // exactly like a truncation recovery — not silently drop the turn to Idle.
    let mut state = fresh_state();
    state
        .session
        .append(ChatMessage::user("original prompt"), state.now);
    state.turn = TurnState::Compacting {
        id: TurnId(7),
        started: std::time::SystemTime::now(),
        trigger: CompactionTrigger::ContextLimitRetry,
        resume_continuation: false,
    };
    let result = fake_recovery_result(vec![ChatMessage::user("compacted context")]);
    let (state, cmds) = update(
        state,
        Msg::CompactionFinished {
            turn: TurnId(7),
            result,
        },
    );

    assert!(
        matches!(state.turn, TurnState::Generating { .. }),
        "context-limit retry resumes generating with the compacted context"
    );
    assert!(
        cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })),
        "re-dispatches the model call to finish the interrupted work"
    );
}

#[test]
fn finished_manual_compaction_still_goes_idle() {
    // Regression guard: only TruncationRecovery resumes; manual /compact ends.
    let mut state = fresh_state();
    state
        .session
        .append(ChatMessage::user("original prompt"), state.now);
    state.turn = TurnState::Compacting {
        id: TurnId(7),
        started: std::time::SystemTime::now(),
        trigger: CompactionTrigger::Manual,
        resume_continuation: false,
    };
    let result = fake_recovery_result(vec![ChatMessage::user("compacted")]);
    let (state, cmds) = update(
        state,
        Msg::CompactionFinished {
            turn: TurnId(7),
            result,
        },
    );

    assert!(matches!(state.turn, TurnState::Idle));
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })));
}

#[test]
fn failed_truncation_recovery_stops_with_hint() {
    let mut state = fresh_state();
    state.session.append(ChatMessage::user("x"), state.now);
    state.turn = TurnState::Compacting {
        id: TurnId(7),
        started: std::time::SystemTime::now(),
        trigger: CompactionTrigger::TruncationRecovery,
        resume_continuation: false,
    };
    let (state, _) = update(
        state,
        Msg::CompactionFailed {
            turn: TurnId(7),
            trigger: CompactionTrigger::TruncationRecovery,
            message: "compaction did not reduce context".to_string(),
            kind: StatusKind::Error,
        },
    );
    assert!(matches!(state.turn, TurnState::Idle));
    assert!(
        state
            .session
            .messages()
            .iter()
            .any(|m| m.content.contains("Response truncated")),
        "a failed recovery falls back to the manual-levers hint, not a raw error"
    );
}

#[test]
fn manual_compaction_skip_shows_calm_note_not_failure() {
    // A manual /compact with nothing to compact (Info kind) is a benign no-op,
    // not a failure: show a calm note, never "Compaction failed: Invalid request".
    let mut state = fresh_state();
    state.session.append(ChatMessage::user("x"), state.now);
    state.turn = TurnState::Compacting {
        id: TurnId(7),
        started: std::time::SystemTime::now(),
        trigger: CompactionTrigger::Manual,
        resume_continuation: false,
    };
    let (state, _) = update(
        state,
        Msg::CompactionFailed {
            turn: TurnId(7),
            trigger: CompactionTrigger::Manual,
            message: "not enough conversation history to summarize".to_string(),
            kind: StatusKind::Info,
        },
    );
    assert!(matches!(state.turn, TurnState::Idle));
    let msgs = state.session.messages();
    assert!(
        msgs.iter()
            .any(|m| m.content.contains("Nothing to compact")),
        "benign skip should show a calm note"
    );
    assert!(
        !msgs
            .iter()
            .any(|m| m.content.contains("Compaction failed")
                || m.content.contains("Invalid request")),
        "benign skip must not read as a failure"
    );
}

#[test]
fn manual_compaction_real_failure_still_says_failed() {
    // Regression guard: a genuine manual-compaction error (Error kind) still
    // surfaces as "Compaction failed: …".
    let mut state = fresh_state();
    state.session.append(ChatMessage::user("x"), state.now);
    state.turn = TurnState::Compacting {
        id: TurnId(7),
        started: std::time::SystemTime::now(),
        trigger: CompactionTrigger::Manual,
        resume_continuation: false,
    };
    let (state, _) = update(
        state,
        Msg::CompactionFailed {
            turn: TurnId(7),
            trigger: CompactionTrigger::Manual,
            message: "compaction produced an empty summary".to_string(),
            kind: StatusKind::Error,
        },
    );
    assert!(matches!(state.turn, TurnState::Idle));
    assert!(
        state
            .session
            .messages()
            .iter()
            .any(|m| m.content.contains("Compaction failed")),
        "a real failure should still say so"
    );
}

#[test]
fn compaction_config_defaults_to_three() {
    let cfg = crate::CompactionConfig::default();
    assert_eq!(cfg.max_truncation_recoveries, 3);
    // An absent [compaction] section deserializes to the default.
    let parsed: crate::Config = toml::from_str("").unwrap();
    assert_eq!(parsed.compaction.max_truncation_recoveries, 3);
}

#[test]
fn fold_token_usage_variants_route_to_the_right_meters() {
    let mut state = fresh_state();
    let usage = mermaid_model::models::TokenUsage::provider(100, 20).with_reasoning_output(5);

    // OwnRequest: last + cumulative, no run banking (the stream-done
    // path banks its own output separately).
    fold_token_usage(
        &mut state.session,
        &mut state.runtime,
        &usage,
        UsageFold::OwnRequest,
    );
    assert_eq!(state.session.last_token_usage.unwrap().total_tokens(), 125);
    assert_eq!(state.session.cumulative_token_usage.total_tokens(), 125);
    assert_eq!(state.runtime.run_tokens.output_tokens, 25);
    assert!(!state.runtime.run_tokens.contains_estimate);

    // Subagent: cumulative + run counter (output only), never last —
    // the child's context is a separate window.
    state.session.last_token_usage = None;
    fold_token_usage(
        &mut state.session,
        &mut state.runtime,
        &usage,
        UsageFold::Subagent,
    );
    assert!(state.session.last_token_usage.is_none());
    assert_eq!(state.session.cumulative_token_usage.total_tokens(), 250);
    assert_eq!(state.runtime.run_tokens.output_tokens, 50);

    // Manual compaction: charged like an own request, but not run spend.
    fold_token_usage(
        &mut state.session,
        &mut state.runtime,
        &usage,
        UsageFold::Compaction { mid_run: false },
    );
    assert_eq!(state.session.last_token_usage.unwrap().total_tokens(), 125);
    assert_eq!(state.session.cumulative_token_usage.total_tokens(), 375);
    assert_eq!(state.runtime.run_tokens.output_tokens, 50);

    // Mid-run (auto/recovery) compaction output IS run spend.
    fold_token_usage(
        &mut state.session,
        &mut state.runtime,
        &usage,
        UsageFold::Compaction { mid_run: true },
    );
    assert_eq!(state.session.cumulative_token_usage.total_tokens(), 500);
    assert_eq!(state.runtime.run_tokens.output_tokens, 75);
}

#[test]
fn stream_done_tracks_last_and_cumulative_token_usage() {
    let mut state = fresh_state();
    state.turn = TurnState::Generating {
        id: TurnId(5),
        started: std::time::SystemTime::now(),
        partial_text: "final answer".to_string(),
        partial_reasoning: String::new(),
        tokens: 0,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: Vec::new(),
        continuation: false,
    };

    let (state, _) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(5),
            usage: Some(mermaid_model::models::TokenUsage::provider(120, 30)),
            provider_continuation: None,
            stop_reason: None,
        },
    );

    assert_eq!(state.session.last_token_usage.unwrap().prompt_tokens, 120);
    assert_eq!(state.session.cumulative_token_usage.total_tokens(), 150);
    assert_eq!(
        state.session.context_usage.as_ref().unwrap().used_tokens,
        150
    );
}

#[test]
fn stream_done_empty_output_with_reasoning_auto_retries() {
    // The reported bug: a reasoning-heavy turn that produced no text and no
    // tool calls must NOT end the run silently — it auto-retries the model
    // call (bounded), without committing an empty assistant message.
    let mut state = fresh_state();
    state.runtime.run_started = Some(std::time::SystemTime::now());
    state.turn = TurnState::Generating {
        id: TurnId(5),
        started: std::time::SystemTime::now(),
        partial_text: String::new(),
        partial_reasoning: "internal thinking ".repeat(50),
        tokens: 0,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: Vec::new(),
        continuation: false,
    };

    let (state, cmds) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(5),
            usage: Some(mermaid_model::models::TokenUsage::provider(100, 0)),
            provider_continuation: None,
            stop_reason: None,
        },
    );

    assert!(
        cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })),
        "a stalled (no-output) turn must re-issue the model call"
    );
    assert!(
        matches!(state.turn, TurnState::Generating { .. }),
        "run should continue in a fresh Generating turn"
    );
    assert_eq!(state.runtime.empty_continuations, 1);
    assert!(
        !state
            .session
            .messages()
            .iter()
            .any(|m| m.role == MessageRole::Assistant && m.content.trim().is_empty()),
        "must not commit an empty assistant message"
    );
    // The tokens the stalled turn spent are still accounted for.
    assert_eq!(state.session.cumulative_token_usage.total_tokens(), 100);
}

#[test]
fn stream_done_empty_output_past_cap_hints_and_ends() {
    // Once the per-run retry budget is spent, a still-empty turn stops the run
    // with a clear hint instead of looping forever.
    let mut state = fresh_state();
    state.runtime.run_started = Some(std::time::SystemTime::now());
    state.runtime.empty_continuations = super::MAX_EMPTY_CONTINUATIONS;
    state.turn = TurnState::Generating {
        id: TurnId(5),
        started: std::time::SystemTime::now(),
        partial_text: String::new(),
        partial_reasoning: "thinking".to_string(),
        tokens: 0,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: Vec::new(),
        continuation: false,
    };

    let (state, cmds) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(5),
            usage: None,
            provider_continuation: None,
            stop_reason: None,
        },
    );

    assert!(
        !cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })),
        "past the cap the run must not keep retrying"
    );
    assert!(matches!(state.turn, TurnState::Idle), "run ends");
    assert!(
        state
            .session
            .messages()
            .iter()
            .any(|m| m.content.contains("no reply or action")),
        "should surface the no-output hint"
    );
}

#[test]
fn stream_done_with_output_resets_empty_continuation_guard() {
    // A turn that makes progress clears the guard so a later stall in the same
    // run gets a full retry budget again.
    let mut state = fresh_state();
    state.runtime.empty_continuations = 1;
    state.turn = TurnState::Generating {
        id: TurnId(5),
        started: std::time::SystemTime::now(),
        partial_text: "here is the answer".to_string(),
        partial_reasoning: String::new(),
        tokens: 0,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: Vec::new(),
        continuation: false,
    };

    let (state, _) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(5),
            usage: None,
            provider_continuation: None,
            stop_reason: None,
        },
    );

    assert_eq!(state.runtime.empty_continuations, 0);
}

#[test]
fn context_usage_estimate_is_stored_during_generation() {
    let mut state = fresh_state();
    state.turn = TurnState::Generating {
        id: TurnId(5),
        started: std::time::SystemTime::now(),
        partial_text: String::new(),
        partial_reasoning: String::new(),
        tokens: 0,
        phase: GenPhase::Thinking,
        provider_continuation: None,
        pending_tool_calls: Vec::new(),
        continuation: false,
    };
    let snapshot = crate::state::ContextUsageSnapshot::from_estimate(
        crate::state::PromptTokenBreakdown {
            system_tokens: 10,
            instructions_tokens: 0,
            message_tokens: 20,
            tool_schema_tokens: 30,
            image_count: 0,
            message_count: 1,
            tool_count: 2,
        },
        Some(1_000),
    );

    let (state, _) = update(
        state,
        Msg::ContextUsageEstimated {
            turn: TurnId(5),
            snapshot,
        },
    );

    let context = state.session.context_usage.expect("context usage");
    assert!(context.is_estimate());
    assert_eq!(context.used_tokens, 60);
    assert_eq!(context.used_percent, Some(6));
}

#[test]
fn context_text_explains_auto_compaction_policy() {
    let mut state = fresh_state();
    state.runtime.provider_capabilities.max_context_tokens = Some(8_000);
    state.session.append(ChatMessage::user("hello"), state.now);

    let text = context_text(&state);

    assert!(text.contains("Next request:"));
    assert!(text.contains("Response reserve:"));
    assert!(text.contains("Auto compact threshold:"));
    assert!(text.contains("Auto compact:"));
    assert!(text.contains("Hard limit risk:"));
}

/// F4 defense-in-depth: if a later refactor weakens the stale
/// filter at the top of `update_step`, `handle_upstream_error`
/// still refuses to mutate state when the error's turn id doesn't
/// match the active turn. Direct-call the helper to exercise the
/// guard without relying on the outer filter.
#[test]
fn handle_upstream_error_refuses_mismatched_turn_id() {
    let mut state = fresh_state();
    state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
    let err = mermaid_model::models::UserFacingError {
        summary: "Stale".to_string(),
        message: "wrong turn".to_string(),
        suggestion: String::new(),
        category: mermaid_model::models::ErrorCategory::Temporary,
        recoverable: true,
    };
    let mut cmds = Vec::new();
    super::handle_upstream_error(&mut state, &mut cmds, TurnId(999), err);
    // Active turn must be untouched and no error message committed.
    assert!(matches!(
        state.turn,
        TurnState::Generating { id: TurnId(5), .. }
    ));
    assert!(state.session.messages().is_empty());
}

#[test]
fn upstream_error_ends_turn_and_records_line() {
    let mut state = fresh_state();
    state.turn = start_generating(TurnId(1), std::time::SystemTime::now());
    let err = mermaid_model::models::UserFacingError {
        summary: "Server error".to_string(),
        message: "500 internal".to_string(),
        suggestion: "retry".to_string(),
        category: mermaid_model::models::ErrorCategory::Temporary,
        recoverable: true,
    };
    let (state, cmds) = update(
        state,
        Msg::UpstreamError {
            turn: TurnId(1),
            error: err,
        },
    );
    assert!(matches!(state.turn, TurnState::Idle));
    // The errored turn persists the session — a headless run's emitted
    // session id must point at a real file even when the provider failed.
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::SaveConversation { .. })),
        "upstream error must save the conversation"
    );
    assert_eq!(state.session.messages().len(), 1);
    let m = &state.session.messages()[0];
    // Error surfaces through the ActionDisplay only — content is
    // intentionally empty so the chat widget doesn't paint the
    // error twice (once as a content line, once as an action).
    assert_eq!(m.content, "");
    assert_eq!(m.actions.len(), 1);
    assert_eq!(m.actions[0].target, "Server error");
}

#[test]
fn upstream_error_drains_queued_message() {
    // A provider error ends the turn; a message the user queued mid-turn
    // must be submitted, not stranded until the next manual prompt (#121).
    let mut state = fresh_state();
    state.turn = start_generating(TurnId(1), std::time::SystemTime::now());
    state
        .ui
        .queued_messages
        .push_back(crate::state::QueuedMessage {
            text: "queued during turn".to_string(),
            attachment_ids: Vec::new(),
        });
    let err = mermaid_model::models::UserFacingError {
        summary: "Server error".to_string(),
        message: "500 internal".to_string(),
        suggestion: "retry".to_string(),
        category: mermaid_model::models::ErrorCategory::Temporary,
        recoverable: true,
    };
    let (state, cmds) = update(
        state,
        Msg::UpstreamError {
            turn: TurnId(1),
            error: err,
        },
    );
    // The queued message was submitted: a fresh turn is generating with a
    // CallModel, and the FIFO is empty.
    assert!(matches!(state.turn, TurnState::Generating { .. }));
    assert!(cmds.iter().any(|cmd| matches!(cmd, Cmd::CallModel { .. })));
    assert!(state.ui.queued_messages.is_empty());
    // Both the error line and the now-submitted queued message are present.
    assert!(
        state
            .session
            .messages()
            .iter()
            .any(|m| m.actions.iter().any(|a| a.target == "Server error"))
    );
    assert!(
        state
            .session
            .messages()
            .iter()
            .any(|m| m.content == "queued during turn")
    );
}

#[test]
fn slash_model_with_arg_persists_and_updates_session() {
    let state = fresh_state();
    let (state, cmds) = update(
        state,
        Msg::Slash(SlashCmd::Model(Some("anthropic/opus".to_string()))),
    );
    assert_eq!(state.session.model_id, "anthropic/opus");
    assert!(cmds.iter().any(|c| matches!(c, Cmd::PersistLastModel(_))));
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::PullOllamaModel { .. }))
    );
}

#[test]
fn slash_model_local_ollama_auto_pulls() {
    let state = fresh_state();
    let (state, cmds) = update(
        state,
        Msg::Slash(SlashCmd::Model(Some("ollama/qwen3:8b".to_string()))),
    );
    assert_eq!(state.session.model_id, "ollama/qwen3:8b");
    assert!(
        cmds.iter()
            .any(|c| { matches!(c, Cmd::PullOllamaModel { model } if model == "qwen3:8b") }),
        "local Ollama model should dispatch pull: {cmds:?}"
    );
}

#[test]
fn slash_model_bare_name_auto_pulls_as_ollama() {
    let state = fresh_state();
    let (_, cmds) = update(
        state,
        Msg::Slash(SlashCmd::Model(Some("qwen3-coder:30b".to_string()))),
    );
    assert!(
        cmds.iter()
            .any(|c| { matches!(c, Cmd::PullOllamaModel { model } if model == "qwen3-coder:30b") }),
        "bare model names should dispatch an Ollama pull: {cmds:?}"
    );
}

#[test]
fn slash_model_ollama_cloud_skips_local_pull() {
    let state = fresh_state();
    let (_, cmds) = update(
        state,
        Msg::Slash(SlashCmd::Model(Some("ollama/gpt-oss:cloud".to_string()))),
    );
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::PullOllamaModel { .. }))
    );
}

#[test]
fn slash_help_appends_system_help_and_persists() {
    let state = fresh_state();
    let (state, cmds) = update(state, Msg::Slash(SlashCmd::Help));
    let msg = state.session.messages().last().expect("help message");
    assert_eq!(msg.role, MessageRole::System);
    assert!(msg.content.contains("Everyday:"));
    assert!(msg.content.contains("Advanced runtime:"));
    assert!(msg.content.contains("/model"));
    assert!(msg.content.contains("/help"));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::SaveConversation { .. }))
    );
}

#[test]
fn slash_doctor_appends_session_readiness_report() {
    let state = fresh_state();
    let (state, cmds) = update(state, Msg::Slash(SlashCmd::Doctor));
    let msg = state.session.messages().last().expect("doctor message");
    assert_eq!(msg.role, MessageRole::System);
    assert!(msg.content.contains("Mermaid Doctor"));
    assert!(msg.content.contains("Active model:"));
    assert!(msg.content.contains("Safety:"));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::SaveConversation { .. }))
    );
}

#[test]
fn slash_memory_commands_dispatch_effects() {
    // /memory lists; /remember <text> and /forget <id> route to effects.
    let (_s, cmds) = update(fresh_state(), Msg::Slash(SlashCmd::Memory));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::ListMemory)));

    let (_s, cmds) = update(
        fresh_state(),
        Msg::Slash(SlashCmd::Remember("prefer ripgrep".to_string())),
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::RememberMemory { text } if text == "prefer ripgrep"))
    );

    let (_s, cmds) = update(
        fresh_state(),
        Msg::Slash(SlashCmd::Forget("prefer-rg".to_string())),
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::ForgetMemory { id } if id == "prefer-rg"))
    );

    // A no-arg /remember never reaches the Remember arm: the parser
    // answers with the registry's usage line, and the reducer prints it.
    let (state, cmds) = update(
        fresh_state(),
        Msg::Slash(crate::parse_slash_command("remember")),
    );
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::RememberMemory { .. })));
    assert!(
        state
            .session
            .messages()
            .last()
            .is_some_and(|m| m.content.contains("Usage: /remember")),
        "no-arg /remember posts a usage hint to the transcript"
    );

    // /consolidate-memory routes to the model-assisted prune effect.
    let (_s, cmds) = update(fresh_state(), Msg::Slash(SlashCmd::ConsolidateMemory));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::ConsolidateMemory { .. }))
    );
}

#[test]
fn chat_request_uses_runtime_prompt_customization() {
    let mut state = fresh_state();
    state.settings.prompt.system_prompt = Some("replacement prompt".to_string());
    state
        .settings
        .prompt
        .append_system_prompt
        .push("extra runtime rule".to_string());

    let request = build_chat_request(&state);
    assert!(request.system_prompt.contains("replacement prompt"));
    assert!(request.system_prompt.contains("extra runtime rule"));
    assert!(!request.system_prompt.contains("Core Loop"));
    assert!(request.system_prompt.contains("Current working directory"));
}

#[test]
fn slash_reasoning_persists_per_model() {
    let state = fresh_state();
    let (state, cmds) = update(
        state,
        Msg::Slash(SlashCmd::Reasoning(Some(
            mermaid_model::models::ReasoningLevel::High,
        ))),
    );
    assert_eq!(
        state.session.reasoning,
        mermaid_model::models::ReasoningLevel::High
    );
    let emitted = cmds
        .iter()
        .find_map(|c| match c {
            Cmd::PersistReasoningFor { model_id, level } => Some((model_id.clone(), *level)),
            _ => None,
        })
        .expect("persist cmd emitted");
    assert_eq!(emitted.0, "ollama/test");
    assert_eq!(emitted.1, mermaid_model::models::ReasoningLevel::High);
}

#[test]
fn slash_context_set_persists_per_model() {
    use crate::ContextCmd;
    let state = fresh_state();
    let (state, cmds) = update(
        state,
        Msg::Slash(SlashCmd::Context(ContextCmd::Set(65_536))),
    );
    assert_eq!(
        state.settings.ollama_num_ctx_per_model.get("ollama/test"),
        Some(&65_536)
    );
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::PersistOllamaNumCtxFor { model_id, num_ctx: Some(65_536) } if model_id == "ollama/test"
    )));
}

#[test]
fn slash_context_auto_clears_override() {
    use crate::ContextCmd;
    let mut state = fresh_state();
    state
        .settings
        .ollama_num_ctx_per_model
        .insert("ollama/test".to_string(), 65_536);
    let (state, cmds) = update(state, Msg::Slash(SlashCmd::Context(ContextCmd::Auto)));
    assert!(
        !state
            .settings
            .ollama_num_ctx_per_model
            .contains_key("ollama/test")
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::PersistOllamaNumCtxFor { num_ctx: None, .. }))
    );
}

#[test]
fn slash_context_offload_toggles_and_persists() {
    use crate::ContextCmd;
    let state = fresh_state();
    let (state, cmds) = update(
        state,
        Msg::Slash(SlashCmd::Context(ContextCmd::Offload(true))),
    );
    assert!(state.settings.ollama.allow_ram_offload);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::PersistOllamaOffload(true)))
    );
}

#[test]
fn build_chat_request_carries_per_model_num_ctx() {
    let mut state = fresh_state();
    state
        .settings
        .ollama_num_ctx_per_model
        .insert("ollama/test".to_string(), 32_768);
    let req = build_chat_request(&state);
    assert_eq!(req.ollama_num_ctx, Some(32_768));
}

#[test]
fn build_chat_request_carries_live_offload_setting() {
    // The provider's config is frozen at startup, so the live offload toggle
    // must ride on the request to take effect on the next turn.
    let mut state = fresh_state();
    assert_eq!(
        build_chat_request(&state).ollama_allow_ram_offload,
        Some(false)
    );
    state.settings.ollama.allow_ram_offload = true;
    assert_eq!(
        build_chat_request(&state).ollama_allow_ram_offload,
        Some(true)
    );
}

#[test]
fn provider_context_resolved_stored_in_runtime() {
    use mermaid_model::models::adapters::ollama_sizing::NumCtxSource;
    let state = fresh_state();
    let (state, _) = update(
        state,
        Msg::ProviderContextResolved {
            model_id: "ollama/test".to_string(),
            model_max: Some(262_144),
            effective: Some(12_288),
            source: Some(NumCtxSource::Auto),
            max_output: Some(64_000),
        },
    );
    let ctx = state.runtime.ollama_context.expect("stored");
    assert_eq!(ctx.model_max, Some(262_144));
    assert_eq!(ctx.effective, Some(12_288));
    // The live values also refresh the capability snapshot (this is what
    // turns `Context: unknown` into a real number for remote providers).
    assert_eq!(
        state.runtime.provider_capabilities.max_context_tokens,
        Some(12_288)
    );
    assert_eq!(
        state.runtime.provider_capabilities.max_output_tokens,
        Some(64_000)
    );
}

#[test]
fn provider_context_resolved_ignores_probe_for_other_model() {
    // A window probe that lands after a /model switch (model_id != session
    // model) must not overwrite the active model's context window.
    use mermaid_model::models::adapters::ollama_sizing::NumCtxSource;
    let state = fresh_state();
    let (state, _) = update(
        state,
        Msg::ProviderContextResolved {
            model_id: "ollama/other".to_string(),
            model_max: Some(262_144),
            effective: Some(12_288),
            source: Some(NumCtxSource::Auto),
            max_output: Some(64_000),
        },
    );
    assert!(state.runtime.ollama_context.is_none());
    // The stale probe must not leak into the capability snapshot either.
    assert_ne!(
        state.runtime.provider_capabilities.max_output_tokens,
        Some(64_000)
    );
}

// A spill with no fitting smaller window (weights-bound) → the warn path.
fn placement_msg(model_id: &str, vram: u64, total: u64) -> Msg {
    Msg::OllamaPlacementResolved {
        model_id: model_id.to_string(),
        size_vram_bytes: vram,
        total_bytes: total,
        suggested_num_ctx: None,
    }
}

fn cpu_warn_count(state: &State) -> usize {
    state
        .session
        .messages()
        .iter()
        .filter(|m| m.role == MessageRole::System && m.content.contains("CPU/RAM"))
        .count()
}

#[test]
fn ollama_placement_stored_and_warns_once_when_offloaded() {
    let state = fresh_state(); // session model is ollama/test; offload off by default
    assert!(!state.settings.ollama.allow_ram_offload);
    let (state, _) = update(
        state,
        placement_msg("ollama/test", 6_000_000_000, 8_000_000_000),
    );
    let p = state.runtime.ollama_placement.expect("stored");
    assert!(p.offloaded());
    assert_eq!(p.percent_on_cpu(), 25);
    assert_eq!(cpu_warn_count(&state), 1);
    assert!(state.runtime.offload_warned.contains("ollama/test"));
    // A second probe for the same model must not warn again.
    let (state, _) = update(
        state,
        placement_msg("ollama/test", 6_000_000_000, 8_000_000_000),
    );
    assert_eq!(cpu_warn_count(&state), 1);
}

#[test]
fn ollama_placement_no_warn_when_offload_on() {
    let mut state = fresh_state();
    state.settings.ollama.allow_ram_offload = true;
    // Fully on CPU, but the user explicitly accepted RAM → silent.
    let (state, _) = update(state, placement_msg("ollama/test", 0, 8_000_000_000));
    assert!(state.runtime.ollama_placement.expect("stored").offloaded());
    assert_eq!(cpu_warn_count(&state), 0);
}

#[test]
fn ollama_placement_no_warn_when_fully_on_gpu() {
    let state = fresh_state();
    let (state, _) = update(
        state,
        placement_msg("ollama/test", 8_000_000_000, 8_000_000_000),
    );
    assert!(!state.runtime.ollama_placement.expect("stored").offloaded());
    assert_eq!(cpu_warn_count(&state), 0);
}

#[test]
fn ollama_placement_ignores_probe_for_other_model() {
    // A probe that lands after a /model switch (model_id != session model).
    let state = fresh_state();
    let (state, _) = update(state, placement_msg("ollama/other", 0, 8_000_000_000));
    assert!(state.runtime.ollama_placement.is_none());
    assert!(!state.runtime.offload_warned.contains("ollama/other"));
    assert_eq!(cpu_warn_count(&state), 0);
}

#[test]
fn ollama_placement_offload_math_boundaries() {
    use mermaid_model::tool_run::OllamaPlacement;
    let p = |vram, total| OllamaPlacement {
        size_vram_bytes: vram,
        total_bytes: total,
    };
    assert!(!p(100, 100).offloaded());
    assert_eq!(p(100, 100).percent_on_cpu(), 0);
    assert!(p(0, 100).offloaded());
    assert_eq!(p(0, 100).percent_on_cpu(), 100);
    assert_eq!(p(75, 100).percent_on_cpu(), 25);
    // vram > total can't really happen, but must not panic / underflow.
    assert!(!p(200, 100).offloaded());
    assert_eq!(p(200, 100).percent_on_cpu(), 0);
    // Unknown footprint → 0, not a divide-by-zero.
    assert_eq!(p(0, 0).percent_on_cpu(), 0);
}

fn converge_msg(model_id: &str, vram: u64, total: u64, suggested: u32) -> Msg {
    Msg::OllamaPlacementResolved {
        model_id: model_id.to_string(),
        size_vram_bytes: vram,
        total_bytes: total,
        suggested_num_ctx: Some(suggested),
    }
}

#[test]
fn ollama_placement_auto_converges_to_suggested_window() {
    // Spilled, but a smaller window fits → adopt it (no warning), and
    // build_chat_request should then send it.
    let state = fresh_state();
    let (state, _) = update(
        state,
        converge_msg("ollama/test", 6_000_000_000, 8_000_000_000, 8_192),
    );
    assert_eq!(
        state.runtime.ollama_converged_num_ctx.get("ollama/test"),
        Some(&8_192)
    );
    assert_eq!(cpu_warn_count(&state), 0); // converged, not warned
    assert!(
        state
            .session
            .messages()
            .iter()
            .any(|m| m.content.contains("Reduced") && m.content.contains("fits your GPU"))
    );
    assert_eq!(build_chat_request(&state).ollama_num_ctx, Some(8_192));
}

#[test]
fn ollama_placement_does_not_converge_when_user_pinned() {
    // The user explicitly set a window → don't auto-resize it; warn instead.
    let mut state = fresh_state();
    state
        .settings
        .ollama_num_ctx_per_model
        .insert("ollama/test".to_string(), 32_768);
    let (state, _) = update(
        state,
        converge_msg("ollama/test", 6_000_000_000, 8_000_000_000, 8_192),
    );
    assert!(
        !state
            .runtime
            .ollama_converged_num_ctx
            .contains_key("ollama/test")
    );
    assert_eq!(cpu_warn_count(&state), 1);
    // Their pinned value still wins.
    assert_eq!(build_chat_request(&state).ollama_num_ctx, Some(32_768));
}

#[test]
fn ollama_placement_never_converges_below_conversation_size() {
    // A fitting window smaller than the live conversation would wedge the
    // session (every turn truncates) — keep the larger window and warn.
    let mut state = fresh_state();
    state
        .session
        .append(ChatMessage::user("word ".repeat(8_000)), state.now); // ≫ 4096 tokens
    let (state, _) = update(
        state,
        converge_msg("ollama/test", 6_000_000_000, 8_000_000_000, 4_096),
    );
    assert!(
        !state
            .runtime
            .ollama_converged_num_ctx
            .contains_key("ollama/test"),
        "must not shrink below the conversation"
    );
    assert_eq!(cpu_warn_count(&state), 1);
    assert_eq!(build_chat_request(&state).ollama_num_ctx, None); // window stays auto-fit
}

#[test]
fn slash_context_auto_clears_converged_value() {
    use crate::ContextCmd;
    let mut state = fresh_state();
    state
        .runtime
        .ollama_converged_num_ctx
        .insert("ollama/test".to_string(), 8_192);
    let (state, _) = update(state, Msg::Slash(SlashCmd::Context(ContextCmd::Auto)));
    assert!(
        !state
            .runtime
            .ollama_converged_num_ctx
            .contains_key("ollama/test")
    );
    assert_eq!(build_chat_request(&state).ollama_num_ctx, None); // back to raw auto-fit
}

#[test]
fn slash_visible_reasoning_toggles_runtime_ui_state() {
    let state = fresh_state();
    let (state, _) = update(state, Msg::Slash(SlashCmd::VisibleReasoning(None)));
    assert!(state.ui.show_reasoning);

    let (state, _) = update(
        state,
        Msg::Slash(SlashCmd::VisibleReasoning(Some("off".to_string()))),
    );
    assert!(!state.ui.show_reasoning);
}

#[test]
fn cycle_safety_walks_by_permissiveness() {
    use mermaid_model::safety::SafetyMode as S;
    // Plan is the strictest position (permissiveness 0), so the walk
    // starts there and wraps back to it — one flat cycle, no side door.
    assert_eq!(cycle_safety(S::Plan), S::ReadOnly);
    assert_eq!(cycle_safety(S::ReadOnly), S::Ask);
    assert_eq!(cycle_safety(S::Ask), S::Auto);
    assert_eq!(cycle_safety(S::Auto), S::FullAccess);
    assert_eq!(cycle_safety(S::FullAccess), S::Plan);
    // Every mode is reachable from every other by cycling.
    let mut seen = vec![S::Ask];
    let mut cur = S::Ask;
    for _ in 0..4 {
        cur = cycle_safety(cur);
        seen.push(cur);
    }
    assert_eq!(cycle_safety(cur), S::Ask, "the cycle closes in 5 steps");
    assert_eq!(seen.len(), 5, "every mode appears exactly once");
}

#[test]
fn shift_tab_cycles_safety_mode() {
    let state = fresh_state();
    let start = state.session.safety_mode;
    let (state, _) = update(
        state,
        Msg::Key(Key {
            code: KeyCode::BackTab,
            modifiers: KeyMods::NONE,
        }),
    );
    assert_eq!(state.session.safety_mode, cycle_safety(start));
}

// ── /model picker ────────────────────────────────────────────────

fn model_choice(id: &str, group: &str) -> crate::state::ModelChoice {
    crate::state::ModelChoice {
        id: id.to_string(),
        group: group.to_string(),
        detail: String::new(),
        ready: true,
    }
}

fn pick_key(code: KeyCode) -> Msg {
    Msg::Key(Key {
        code,
        modifiers: KeyMods::default(),
    })
}

/// `/model` with no argument opens the picker and asks for discovery. It
/// used to just print the current model, which answered a question nobody
/// had — the whole point of the command is to CHANGE the model.
#[test]
fn slash_model_opens_the_picker_and_requests_discovery() {
    let (state, cmds) = update(fresh_state(), Msg::Slash(SlashCmd::Model(None)));
    assert!(
        matches!(state.ui.mode, UiMode::ModelPicker { loading: true, .. }),
        "expected a loading picker, got {:?}",
        state.ui.mode
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Query(Query::ListAvailableModels))),
        "the picker must ask for discovery"
    );
    assert!(
        state.session.messages().is_empty(),
        "opening a picker adds no transcript row"
    );
}

/// Discovery lands, the user narrows, and Enter switches — the same path
/// `/model <id>` takes, so the vision re-probe and persistence still fire.
#[test]
fn model_picker_filters_then_switches_on_enter() {
    let (state, _) = update(fresh_state(), Msg::Slash(SlashCmd::Model(None)));
    let (state, _) = update(
        state,
        Msg::QueryResult(QueryResult::AvailableModelsListed(vec![
            model_choice("ollama/llama3.2", "Local (Ollama)"),
            model_choice("anthropic/claude-opus-4-5", "anthropic"),
            model_choice("anthropic/claude-haiku-4-5", "anthropic"),
        ])),
    );
    assert!(matches!(
        state.ui.mode,
        UiMode::ModelPicker { loading: false, .. }
    ));

    // Subsequence match: "ohaik" reaches anthropic/claude-haiku-4-5.
    let mut state = state;
    for c in "ohaik".chars() {
        let (next, _) = update(state, pick_key(KeyCode::Char(c)));
        state = next;
    }
    let UiMode::ModelPicker {
        ref candidates,
        ref query,
        ..
    } = state.ui.mode
    else {
        panic!("picker closed unexpectedly");
    };
    let matches = filter_model_choices(candidates, query);
    assert_eq!(
        matches.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        vec!["anthropic/claude-haiku-4-5"],
    );

    let (state, cmds) = update(state, pick_key(KeyCode::Enter));
    assert_eq!(state.session.model_id, "anthropic/claude-haiku-4-5");
    assert!(
        matches!(state.ui.mode, UiMode::EditingInput),
        "picker closes"
    );
    assert!(
        cmds.iter().any(|c| matches!(c, Cmd::ProbeVision { .. })),
        "switching re-probes vision"
    );
    assert!(
        cmds.iter().any(|c| matches!(c, Cmd::PersistLastModel(_))),
        "switching persists the choice"
    );
}

/// Esc leaves the model untouched, and the draft in the composer survives
/// — the picker never wrote to the input buffer.
/// A paste while the picker is open must filter it, not type into the
/// composer hidden behind it.
///
/// Found by a flaky golden-frame test: typing a filter quickly produced a
/// query and a composer draft with the characters *interleaved*
/// (`filter=zomch`, `composer=zzznat`). The event source coalesces rapid
/// keystrokes into `Msg::Paste`, which went straight to the input buffer —
/// so how much of a filter survived depended on typing speed. An actual
/// Ctrl+V paste always lost the whole thing.
#[test]
fn a_paste_filters_the_model_picker_rather_than_the_hidden_composer() {
    let (state, _) = update(fresh_state(), Msg::Slash(SlashCmd::Model(None)));
    let (state, _) = update(
        state,
        Msg::QueryResult(QueryResult::AvailableModelsListed(vec![
            model_choice("ollama/llama3.2", "Local (Ollama)"),
            model_choice("anthropic/claude-opus-4-5", "anthropic"),
        ])),
    );
    // A burst (what the coalescer produces from fast typing).
    let (state, _) = update(
        state,
        Msg::Paste(crate::msg::Paste::Text("opus".to_string())),
    );
    let UiMode::ModelPicker {
        ref candidates,
        ref query,
        ..
    } = state.ui.mode
    else {
        panic!("the picker must stay open across a paste");
    };
    assert_eq!(query, "opus", "the paste filters the pane");
    assert_eq!(
        filter_model_choices(candidates, query)
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        vec!["anthropic/claude-opus-4-5"],
    );
    assert!(
        state.ui.input_buffer.is_empty(),
        "nothing may leak into the composer behind the pane: {:?}",
        state.ui.input_buffer
    );
}

#[test]
fn model_picker_escape_keeps_the_model_and_the_draft() {
    let (state, _) = type_text(fresh_state(), "half-written prompt");
    let before = state.session.model_id.clone();
    let (state, _) = update(state, Msg::Slash(SlashCmd::Model(None)));
    let (state, cmds) = update(state, pick_key(KeyCode::Escape));
    assert!(matches!(state.ui.mode, UiMode::EditingInput));
    assert_eq!(state.session.model_id, before);
    assert_eq!(state.ui.input_buffer, "half-written prompt");
    assert!(
        !cmds.iter().any(|c| matches!(c, Cmd::PersistLastModel(_))),
        "cancelling must not persist anything"
    );
}

/// A discovery result that lands after the user already dismissed the
/// picker is dropped, not resurrected into a reopened pane.
#[test]
fn late_discovery_after_escape_is_ignored() {
    let (state, _) = update(fresh_state(), Msg::Slash(SlashCmd::Model(None)));
    let (state, _) = update(state, pick_key(KeyCode::Escape));
    let (state, _) = update(
        state,
        Msg::QueryResult(QueryResult::AvailableModelsListed(vec![model_choice(
            "ollama/x",
            "Local (Ollama)",
        )])),
    );
    assert!(matches!(state.ui.mode, UiMode::EditingInput));
}

#[test]
fn model_filter_is_a_case_insensitive_subsequence() {
    let all = vec![
        model_choice("ollama/llama3.2", "Local (Ollama)"),
        model_choice("anthropic/claude-opus-4-5", "anthropic"),
    ];
    let ids = |q: &str| {
        filter_model_choices(&all, q)
            .iter()
            .map(|c| c.id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(""),
        vec!["ollama/llama3.2", "anthropic/claude-opus-4-5"]
    );
    assert_eq!(ids("OPUS"), vec!["anthropic/claude-opus-4-5"]);
    assert_eq!(ids("llama"), vec!["ollama/llama3.2"]);
    // Subsequence, not substring.
    assert_eq!(ids("aoi"), vec!["anthropic/claude-opus-4-5"]);
    assert!(ids("zzz").is_empty());
}

#[test]
fn slash_safety_sets_session_mode() {
    let state = fresh_state();
    let (state, _) = update(
        state,
        Msg::Slash(SlashCmd::Safety(Some(
            mermaid_model::safety::SafetyMode::Auto,
        ))),
    );
    assert_eq!(
        state.session.safety_mode,
        mermaid_model::safety::SafetyMode::Auto
    );
}

/// A tool-result shaped exactly like a real read-only policy denial
/// (`{summary} blocked by policy: {marker} blocks …`), built from the shared
/// marker so the test exercises the real detection path.
fn readonly_denial_message(summary: &str) -> ChatMessage {
    ChatMessage::tool(
        "call-x",
        "execute_command",
        format!(
            "{summary} blocked by policy: {} blocks mutations and control actions",
            mermaid_model::safety::READ_ONLY_DENIAL_MARKER
        ),
    )
}

#[test]
fn superseded_readonly_denial_is_rewritten_when_mode_loosened() {
    use mermaid_model::safety::SafetyMode;
    let mut msgs = vec![readonly_denial_message("write_file(main.qml)")];
    neutralize_superseded_policy_denials(&mut msgs, SafetyMode::FullAccess);
    let content = &msgs[0].content;
    assert!(
        !content.contains("blocked by policy"),
        "standing denial phrasing should be gone: {content:?}"
    );
    assert!(
        content.contains("write_file(main.qml)"),
        "action summary must be kept: {content:?}"
    );
    assert!(
        content.contains("no longer applies"),
        "should read as lifted: {content:?}"
    );
    assert!(
        content.contains("full_access"),
        "should name the now-current mode: {content:?}"
    );
}

#[test]
fn readonly_denial_preserved_in_read_only_mode() {
    use mermaid_model::safety::SafetyMode;
    let original = readonly_denial_message("write_file(x)");
    let mut msgs = vec![original.clone()];
    neutralize_superseded_policy_denials(&mut msgs, SafetyMode::ReadOnly);
    assert_eq!(
        msgs[0].content, original.content,
        "a still-valid denial must be untouched in read_only"
    );
}

#[test]
fn neutralizer_ignores_non_tool_and_non_denial_messages() {
    use mermaid_model::safety::SafetyMode;
    // Role gate: a USER message quoting the full signature is left alone.
    let quote = ChatMessage::user(format!(
        "it said: blocked by policy: {} blocks mutations and control actions",
        mermaid_model::safety::READ_ONLY_DENIAL_MARKER
    ));
    // Contiguous-signature gate: a tool result that merely contains the
    // marker text (e.g. a grep of the source) is not a denial.
    let grep = ChatMessage::tool(
        "c",
        "execute_command",
        format!(
            "policy.rs: const MARKER = {:?};",
            mermaid_model::safety::READ_ONLY_DENIAL_MARKER
        ),
    );
    let mut msgs = vec![quote.clone(), grep.clone()];
    neutralize_superseded_policy_denials(&mut msgs, SafetyMode::FullAccess);
    assert_eq!(msgs[0].content, quote.content, "user message untouched");
    assert_eq!(
        msgs[1].content, grep.content,
        "non-denial tool result untouched"
    );
}

#[test]
fn leaving_read_only_past_a_stale_denial_injects_a_hidden_nudge() {
    use mermaid_model::safety::SafetyMode;
    let mut state = fresh_state();
    state.session.safety_mode = SafetyMode::ReadOnly;
    state
        .session
        .append(readonly_denial_message("write_file(x)"), state.now);
    let (state, _) = update(state, key(KeyCode::BackTab));
    assert_eq!(state.session.safety_mode, SafetyMode::Ask);
    let nudge = state
        .session
        .messages()
        .iter()
        .find(|m| m.content.contains("Safety mode is now ask"))
        .expect("leaving read_only past a stale denial should inject the nudge");
    assert_eq!(nudge.role, MessageRole::System);
    assert_eq!(
        nudge.kind,
        mermaid_model::models::ChatMessageKind::RecoveryNudge,
        "the nudge is for the model only — RecoveryNudge hides it from the transcript",
    );
}

#[test]
fn further_loosening_replaces_the_pending_nudge_instead_of_stacking() {
    use mermaid_model::safety::SafetyMode;
    let mut state = fresh_state();
    state.session.safety_mode = SafetyMode::ReadOnly;
    state
        .session
        .append(readonly_denial_message("write_file(x)"), state.now);
    // The screenshot bug: cycling read_only → ask → auto → full_access
    // announced three times. Now the pending nudge is carried forward,
    // renamed to the current mode.
    let (state, _) = update(state, key(KeyCode::BackTab));
    let (state, _) = update(state, key(KeyCode::BackTab));
    let (state, _) = update(state, key(KeyCode::BackTab));
    assert_eq!(state.session.safety_mode, SafetyMode::FullAccess);
    let nudges: Vec<_> = state
        .session
        .messages()
        .iter()
        .filter(|m| m.content.starts_with(SAFETY_NUDGE_PREFIX))
        .collect();
    assert_eq!(nudges.len(), 1, "exactly one pending nudge, never a stack");
    assert!(
        nudges[0].content.contains("full_access"),
        "the surviving nudge names the current mode: {:?}",
        nudges[0].content
    );
}

#[test]
fn loosening_after_the_nudge_was_spent_stays_silent() {
    use mermaid_model::safety::SafetyMode;
    let mut state = fresh_state();
    state.session.safety_mode = SafetyMode::ReadOnly;
    state
        .session
        .append(readonly_denial_message("write_file(x)"), state.now);
    let (mut state, _) = update(state, key(KeyCode::BackTab)); // → ask, nudge pending
    sweep_spent_nudges(&mut state); // the request it steered went out
    let (state, _) = update(state, key(KeyCode::BackTab)); // ask → auto
    assert!(
        !state
            .session
            .messages()
            .iter()
            .any(|m| m.content.starts_with(SAFETY_NUDGE_PREFIX)),
        "a later loosening must not re-announce a spent nudge",
    );
}

#[test]
fn tightening_back_to_read_only_retracts_the_pending_nudge() {
    use mermaid_model::safety::SafetyMode;
    let mut state = fresh_state();
    state.session.safety_mode = SafetyMode::ReadOnly;
    state
        .session
        .append(readonly_denial_message("write_file(x)"), state.now);
    let (state, _) = update(state, key(KeyCode::BackTab)); // → ask, nudge pending
    let (state, _) = update(
        state,
        Msg::Slash(SlashCmd::Safety(Some(SafetyMode::ReadOnly))),
    );
    assert!(
        !state
            .session
            .messages()
            .iter()
            .any(|m| m.content.starts_with(SAFETY_NUDGE_PREFIX)),
        "back in read_only the denials stand again — the lifted-note must not ride the next request",
    );
}

#[test]
fn clean_mode_cycle_stays_silent() {
    use mermaid_model::safety::SafetyMode;
    let mut state = fresh_state();
    state.session.safety_mode = SafetyMode::ReadOnly;
    let (state, _) = update(state, key(KeyCode::BackTab));
    assert_eq!(state.session.safety_mode, SafetyMode::Ask);
    assert!(
        !state
            .session
            .messages()
            .iter()
            .any(|m| m.content.contains("Safety mode is now")),
        "a clean mode cycle must not add a banner",
    );
}

#[test]
fn slash_safety_loosening_announces_when_denial_present() {
    use mermaid_model::safety::SafetyMode;
    let mut state = fresh_state();
    state.session.safety_mode = SafetyMode::ReadOnly;
    state
        .session
        .append(readonly_denial_message("edit(main.qml)"), state.now);
    let (state, _) = update(
        state,
        Msg::Slash(SlashCmd::Safety(Some(SafetyMode::FullAccess))),
    );
    assert_eq!(state.session.safety_mode, SafetyMode::FullAccess);
    assert!(
        state
            .session
            .messages()
            .iter()
            .any(|m| m.role == MessageRole::System
                && m.content.contains("Safety mode is now full_access")),
    );
}

#[test]
fn build_chat_request_neutralizes_a_superseded_denial() {
    use mermaid_model::safety::SafetyMode;
    // Full production path: a read_only denial sits in history behind a valid
    // tool_use/tool_result pair (so `normalize_history` keeps it); once the
    // live mode is looser, `build_chat_request` must hand the model a
    // rewritten, non-standing note rather than the original block.
    let mut state = fresh_state();
    state.session.safety_mode = SafetyMode::FullAccess;
    let call = mermaid_model::models::tool_call::ToolCall {
        id: Some("call-1".to_string()),
        function: mermaid_model::models::tool_call::FunctionCall {
            name: "write_file".to_string(),
            arguments: serde_json::json!({ "path": "main.qml" }),
        },
    };
    state.session.append(
        ChatMessage::assistant("editing").with_tool_calls(vec![call]),
        state.now,
    );
    state.session.append(
        ChatMessage::tool(
            "call-1",
            "write_file",
            format!(
                "write_file(main.qml) blocked by policy: {} blocks mutations and control actions",
                mermaid_model::safety::READ_ONLY_DENIAL_MARKER
            ),
        ),
        state.now,
    );

    let req = build_chat_request(&state);
    let tool_msg = req
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .expect("the tool_result should survive into the request");
    assert!(
        !tool_msg.content.contains("blocked by policy"),
        "build_chat_request must neutralize the stale denial: {:?}",
        tool_msg.content
    );
    assert!(
        tool_msg.content.contains("no longer applies"),
        "rewritten note expected: {:?}",
        tool_msg.content
    );
}

/// State with one queued approval (turn must accept the message, so put
/// the reducer in a live turn first).
fn pending_approval_state() -> State {
    let mut state = fresh_state();
    state.turn = start_generating(TurnId(1), std::time::SystemTime::now());
    let (state, _) = update(
        state,
        Msg::ApprovalRequested {
            turn: TurnId(1),
            call_id: mermaid_model::ids::ToolCallId(5),
            tool: "execute_command".to_string(),
            risk: "shell_mutation".to_string(),
            kind: crate::ApprovalKind::Shell,
            prompt: "$ npm test".to_string(),
            allowlist_scope: "execute_command:npm".to_string(),
        },
    );
    state
}

fn key(code: KeyCode) -> Msg {
    Msg::Key(Key {
        code,
        modifiers: KeyMods::NONE,
    })
}

#[test]
fn ctrl_b_backgrounds_running_tool() {
    let ctrl_b = Msg::Key(Key {
        code: KeyCode::Char('b'),
        modifiers: KeyMods {
            ctrl: true,
            ..KeyMods::NONE
        },
    });
    // While tools are executing → emit BackgroundScope(turn).
    let mut state = fresh_state();
    state.turn = start_executing_tools(TurnId(9), Vec::new(), std::time::SystemTime::now());
    let (_s, cmds) = update(state, ctrl_b.clone());
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::BackgroundScope(t) if *t == TurnId(9))),
        "Ctrl+B during tool execution should background the scope"
    );
    // Idle → swallowed, no BackgroundScope.
    let (_s, cmds) = update(fresh_state(), ctrl_b);
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::BackgroundScope(_))));
}

#[test]
fn theme_command_switches_persists_and_reports() {
    use crate::ThemeChoice;
    // /theme light → state flips, persist emitted, confirmation appended.
    let (state, cmds) = update(
        fresh_state(),
        Msg::Slash(SlashCmd::Theme(Some("light".to_string()))),
    );
    assert_eq!(state.ui.theme, ThemeChoice::Light);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::PersistUiTheme(ThemeChoice::Light)))
    );
    // /theme (no arg) → reports current, persists nothing.
    let (state, cmds) = update(state, Msg::Slash(SlashCmd::Theme(None)));
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::PersistUiTheme(_))));
    let last = state.session.messages().last().unwrap().content.clone();
    assert!(last.contains("light"), "shows current theme: {last}");
    // Bad arg → usage, no state change, no persist.
    let (state, cmds) = update(
        state,
        Msg::Slash(SlashCmd::Theme(Some("solarized".to_string()))),
    );
    assert_eq!(state.ui.theme, ThemeChoice::Light);
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::PersistUiTheme(_))));
    let last = state.session.messages().last().unwrap().content.clone();
    assert!(last.contains("Usage: /theme"), "usage line: {last}");
}

#[test]
fn theme_command_notes_no_color() {
    let mut state = fresh_state();
    state.ui.no_color = true;
    let (state, cmds) = update(
        state,
        Msg::Slash(SlashCmd::Theme(Some("light".to_string()))),
    );
    // Still persists (applies when NO_COLOR is unset) but says so.
    assert!(cmds.iter().any(|c| matches!(c, Cmd::PersistUiTheme(_))));
    let last = state.session.messages().last().unwrap().content.clone();
    assert!(last.contains("NO_COLOR"), "notes NO_COLOR: {last}");
}

#[test]
fn ctrl_o_composes_draft_in_editor() {
    let ctrl_o = Msg::Key(Key {
        code: KeyCode::Char('o'),
        modifiers: KeyMods {
            ctrl: true,
            ..KeyMods::NONE
        },
    });
    // Idle with a draft → emits ComposeInEditor carrying it.
    let mut state = fresh_state();
    state.ui.input_buffer = "half-typed prompt".to_string();
    let (_s, cmds) = update(state, ctrl_o.clone());
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::ComposeInEditor { text } if text == "half-typed prompt"))
    );
    // Busy (generating) → still allowed; it only edits the draft.
    let mut state = fresh_state();
    state.turn = start_generating(TurnId(3), std::time::SystemTime::now());
    let (_s, cmds) = update(state, ctrl_o.clone());
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::ComposeInEditor { .. }))
    );
    // Over a modal surface (model list) → swallowed.
    let mut state = fresh_state();
    state.ui.mode = UiMode::ModelList;
    let (_s, cmds) = update(state, ctrl_o);
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::ComposeInEditor { .. }))
    );
    // /editor also routes to the compose command.
    let (_s, cmds) = update(fresh_state(), Msg::Slash(SlashCmd::Editor));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::ComposeInEditor { .. }))
    );
}

#[test]
fn editor_returned_replaces_draft() {
    let mut state = fresh_state();
    state.ui.input_buffer = "old draft".to_string();
    state.ui.input_cursor = 3;
    let (state, cmds) = update(
        state,
        Msg::EditorReturned {
            text: Some("new draft from vim".to_string()),
        },
    );
    assert_eq!(state.ui.input_buffer, "new draft from vim");
    assert_eq!(state.ui.input_cursor, state.ui.input_buffer.len());
    assert!(cmds.is_empty());
    // Empty Some = deliberate clear.
    let (state, _) = update(
        state,
        Msg::EditorReturned {
            text: Some(String::new()),
        },
    );
    assert!(state.ui.input_buffer.is_empty());
    // None = no-op.
    let mut state = fresh_state();
    state.ui.input_buffer = "kept".to_string();
    let (state, _) = update(state, Msg::EditorReturned { text: None });
    assert_eq!(state.ui.input_buffer, "kept");
}

fn plugin_cmd(name: &str, body: &str) -> crate::PluginCommand {
    crate::PluginCommand {
        name: name.to_string(),
        description: "does things".to_string(),
        body: body.to_string(),
        plugin: "demo".to_string(),
    }
}

#[test]
fn plugin_command_expands_into_a_prompt_submit() {
    let mut state = fresh_state();
    state.plugin_commands = vec![plugin_cmd("deploy", "Deploy to $ARGUMENTS now.")];
    state.ui.input_buffer = "/deploy prod".to_string();
    let (mut state, _) = update(state, key(KeyCode::Enter));
    // The reducer re-enters pending_msgs itself; the expansion lands as a
    // committed user message (transcript shows the EXPANSION, so
    // recordings replay without the plugin installed).
    let last_user = state
        .session
        .messages()
        .iter()
        .rev()
        .find(|m| m.role == mermaid_model::models::MessageRole::User)
        .map(|m| m.content.clone());
    assert_eq!(last_user.as_deref(), Some("Deploy to prod now."));
    assert!(state.ui.input_buffer.is_empty());
    // No-args + no token: body submits verbatim.
    state.turn = crate::TurnState::Idle;
    state.plugin_commands = vec![plugin_cmd("ship", "Ship it.")];
    state.ui.input_buffer = "/ship".to_string();
    let (state, _) = update(state, key(KeyCode::Enter));
    let queued_or_committed = state
        .session
        .messages()
        .iter()
        .any(|m| m.content == "Ship it.")
        || state
            .ui
            .queued_messages
            .iter()
            .any(|q| q.text == "Ship it.");
    assert!(queued_or_committed, "plugin body submitted or queued");
}

#[test]
fn unknown_slash_still_reports_unknown_not_plugin() {
    let mut state = fresh_state();
    state.plugin_commands = vec![plugin_cmd("deploy", "body")];
    state.ui.input_buffer = "/nosuch".to_string();
    let (state, _) = update(state, key(KeyCode::Enter));
    let last = state.session.messages().last().unwrap().content.clone();
    assert!(last.contains("Unknown command: /nosuch"), "{last}");
}

#[test]
fn builtin_wins_over_same_named_plugin_command() {
    // Structural guarantee on top of the loader's shadowing filter.
    let mut state = fresh_state();
    state.plugin_commands = vec![plugin_cmd("help", "hijacked")];
    state.ui.input_buffer = "/help".to_string();
    let (state, _) = update(state, key(KeyCode::Enter));
    let last = state.session.messages().last().unwrap().content.clone();
    assert!(
        last.contains("Mermaid commands"),
        "built-in help ran: {last}"
    );
    assert!(!last.contains("hijacked"));
}

#[test]
fn palette_filter_entries_appends_plugins_and_agrees_on_indices() {
    use crate::slash_commands::{COMMAND_REGISTRY, filter_entries};
    let plugin = vec![plugin_cmd("deploy", "body")];
    let all = filter_entries("", &plugin);
    assert_eq!(all.len(), COMMAND_REGISTRY.len() + 1);
    assert_eq!(all.last().unwrap().name(), "deploy");
    assert!(all.last().unwrap().description().contains("(plugin:demo)"));
    // Prefix filtering reaches plugin rows too.
    let d = filter_entries("dep", &plugin);
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].name(), "deploy");
    // Tab-completion path: cursor over the plugin row completes it.
    let mut state = fresh_state();
    state.plugin_commands = plugin;
    state.ui.input_buffer = "/dep".to_string();
    state.ui.palette_cursor = Some(0);
    let (state, _) = update(state, key(KeyCode::Tab));
    assert_eq!(state.ui.input_buffer, "/deploy ");
}

#[test]
fn help_lists_plugin_commands() {
    let mut state = fresh_state();
    state.plugin_commands = vec![plugin_cmd("deploy", "body")];
    let (state, _) = update(state, Msg::Slash(SlashCmd::Help));
    let last = state.session.messages().last().unwrap().content.clone();
    assert!(last.contains("Plugin commands:"), "{last}");
    assert!(
        last.contains("/deploy - does things (plugin:demo)"),
        "{last}"
    );
}

#[test]
fn plugin_command_expand_cases() {
    let cmd = plugin_cmd("x", "Do $ARGUMENTS and $ARGUMENTS.");
    assert_eq!(cmd.expand("this"), "Do this and this.");
    assert_eq!(cmd.expand("  "), "Do  and .");
    let cmd = plugin_cmd("x", "Just do it.");
    assert_eq!(cmd.expand(""), "Just do it.");
    assert_eq!(cmd.expand("with args"), "Just do it.\n\nwith args");
}

#[test]
fn paste_interleaved_with_keys_preserves_order() {
    // Reproduces the Windows paste scramble: a paste burst splits into
    // stray Char keys + coalesced Paste chunks. Both must insert at the
    // cursor and advance it, so the result stays in order regardless of
    // the split. (Before the fix, Paste appended to the end while keys
    // inserted at a never-advanced cursor, yielding "RDeview the Docs".)
    let mut state = fresh_state();
    for msg in [
        key(KeyCode::Char('R')),
        Msg::Paste(Paste::Text("eview the ".to_string())),
        key(KeyCode::Char('D')),
        Msg::Paste(Paste::Text("ocs".to_string())),
    ] {
        let (next, _) = update(state, msg);
        state = next;
    }
    assert_eq!(state.ui.input_buffer, "Review the Docs");
    assert_eq!(state.ui.input_cursor, state.ui.input_buffer.len());
}

#[test]
fn paste_inserts_at_cursor_not_end() {
    // Type "ac", move left one, paste "b" → "abc" (not "acb").
    let mut state = fresh_state();
    for msg in [
        key(KeyCode::Char('a')),
        key(KeyCode::Char('c')),
        key(KeyCode::Left),
        Msg::Paste(Paste::Text("b".to_string())),
    ] {
        let (next, _) = update(state, msg);
        state = next;
    }
    assert_eq!(state.ui.input_buffer, "abc");
}

#[test]
fn approval_requested_enqueues_modal() {
    let state = pending_approval_state();
    assert_eq!(state.pending_approval.len(), 1);
    assert_eq!(
        state.pending_approval.front().unwrap().tool,
        "execute_command"
    );
}

#[test]
fn approval_requested_during_cancelling_is_dropped() {
    // #74: a tool task unwinding under cancellation can still emit an
    // ApprovalRequested; parking a modal for it would outlive the turn.
    let mut state = fresh_state();
    state.turn = TurnState::Cancelling {
        id: TurnId(1),
        since: std::time::SystemTime::now(),
    };
    let (state, _) = update(
        state,
        Msg::ApprovalRequested {
            turn: TurnId(1),
            call_id: mermaid_model::ids::ToolCallId(5),
            tool: "execute_command".to_string(),
            risk: "shell_mutation".to_string(),
            kind: crate::ApprovalKind::Shell,
            prompt: "$ rm -rf /".to_string(),
            allowlist_scope: "execute_command:rm".to_string(),
        },
    );
    assert!(
        state.pending_approval.is_empty(),
        "approval for a cancelling turn must not be queued (#74)"
    );
}

#[test]
fn copy_selection_emits_clipboard_cmd_when_nonempty() {
    // #18: the copy side effect flows through the reducer as a Cmd.
    let (_s, cmds) = update(fresh_state(), Msg::CopySelection("hello".to_string()));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::CopyToClipboard(t) if t == "hello")),
        "non-empty selection should emit CopyToClipboard"
    );
    // An empty selection is a no-op — no clipboard Cmd.
    let (_s, cmds) = update(fresh_state(), Msg::CopySelection(String::new()));
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::CopyToClipboard(_))));
}

#[test]
fn approval_keys_emit_the_right_decision() {
    use crate::ApprovalChoice as A;
    for (code, expected) in [
        (KeyCode::Char('1'), A::Approve),
        (KeyCode::Char('y'), A::Approve),
        (KeyCode::Enter, A::Approve),
        (KeyCode::Char('2'), A::ApproveAlways),
        (KeyCode::Char('a'), A::ApproveAlways),
        (KeyCode::Char('3'), A::Deny),
        (KeyCode::Char('n'), A::Deny),
        (KeyCode::Escape, A::Deny),
    ] {
        let (state, cmds) = update(pending_approval_state(), key(code));
        assert!(
            state.pending_approval.is_empty(),
            "{code:?} should pop the modal"
        );
        assert!(
            cmds.iter().any(
                |c| matches!(c, Cmd::ResolveApproval { decision, .. } if *decision == expected)
            ),
            "{code:?} should resolve {expected:?}; got {cmds:?}",
        );
        // Esc on an approval denies the tool — it must NOT cancel the turn.
        if code == KeyCode::Escape {
            assert!(
                !cmds.iter().any(|c| matches!(c, Cmd::CancelScope(_))),
                "Esc on an approval must deny, not cancel the turn",
            );
        }
    }
}

#[test]
fn approval_modal_swallows_unrelated_keys() {
    let (state, cmds) = update(pending_approval_state(), key(KeyCode::Char('x')));
    assert_eq!(
        state.pending_approval.len(),
        1,
        "unrelated key must not pop the modal"
    );
    assert!(cmds.is_empty());
}

#[test]
fn approval_arrows_move_highlight_without_resolving() {
    // ↓ moves the highlight and clamps at the last option; ↑ moves back.
    // Neither resolves the modal.
    let (state, cmds) = update(pending_approval_state(), key(KeyCode::Down));
    assert_eq!(state.pending_approval.front().unwrap().selected_option, 1);
    assert!(cmds.is_empty() && state.pending_approval.len() == 1);

    let (state, _) = update(state, key(KeyCode::Down));
    assert_eq!(state.pending_approval.front().unwrap().selected_option, 2);
    let (state, _) = update(state, key(KeyCode::Down)); // clamps at 2
    assert_eq!(state.pending_approval.front().unwrap().selected_option, 2);
    let (state, _) = update(state, key(KeyCode::Up));
    assert_eq!(state.pending_approval.front().unwrap().selected_option, 1);
}

#[test]
fn approval_enter_resolves_the_highlighted_option() {
    use crate::ApprovalChoice as A;
    // Highlight option 3 (No) with two ↓, then Enter → Deny.
    let (state, _) = update(pending_approval_state(), key(KeyCode::Down));
    let (state, _) = update(state, key(KeyCode::Down));
    let (state, cmds) = update(state, key(KeyCode::Enter));
    assert!(
        state.pending_approval.is_empty(),
        "Enter should pop the modal"
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::ResolveApproval { decision, .. } if *decision == A::Deny)),
        "Enter on the highlighted 'No' must deny; got {cmds:?}"
    );
}

#[test]
fn approval_fifo_shows_one_at_a_time() {
    let state = pending_approval_state();
    let (state, _) = update(
        state,
        Msg::ApprovalRequested {
            turn: TurnId(1),
            call_id: mermaid_model::ids::ToolCallId(6),
            tool: "write_file".to_string(),
            risk: "file_mutation".to_string(),
            kind: crate::ApprovalKind::FileMutation,
            prompt: "src/x.rs".to_string(),
            allowlist_scope: "write_file".to_string(),
        },
    );
    assert_eq!(state.pending_approval.len(), 2);
    let (state, _) = update(state, key(KeyCode::Char('1')));
    assert_eq!(state.pending_approval.len(), 1);
    assert_eq!(state.pending_approval.front().unwrap().tool, "write_file");
}

#[test]
fn clear_confirm_now_accepts_via_keypress() {
    // Regression: the /clear confirmation was inert (never rendered, never
    // key-handled). It now resolves on a keypress.
    let mut state = fresh_state();
    state.confirm = Some(crate::state::Confirmation {
        prompt: "Clear conversation history?".to_string(),
        accept_msg_token: crate::state::ConfirmationTarget::ClearConversation,
    });
    let (state, _) = update(state, key(KeyCode::Char('y')));
    assert!(
        state.confirm.is_none(),
        "y should accept and clear the confirm modal"
    );
}

#[test]
fn slash_clear_raises_confirmation() {
    let state = fresh_state();
    let (state, _) = update(state, Msg::Slash(SlashCmd::Clear));
    assert!(state.confirm.is_some());
}

#[test]
fn confirm_accepted_for_clear_wipes_messages() {
    let mut state = fresh_state();
    state.session.append(ChatMessage::user("one"), state.now);
    state
        .session
        .append(ChatMessage::assistant("two"), state.now);
    state.confirm = Some(crate::state::Confirmation {
        prompt: "Clear conversation history?".to_string(),
        accept_msg_token: crate::state::ConfirmationTarget::ClearConversation,
    });
    let (state, _) = update(state, Msg::ConfirmAccepted);
    assert!(state.session.messages().is_empty());
    assert!(state.confirm.is_none());
}

/// `/clear` used to blank the gauge to `context: n/a`, which reads as
/// "unknown". A cleared conversation is not unknown — the system prompt and
/// every advertised tool schema still ride the next request, so the floor is
/// known, non-zero, and worth seeing before you type. Same treatment as a
/// rewind; cumulative spend still resets, because that is a different number.
#[test]
fn clearing_re_estimates_the_context_gauge_instead_of_blanking_it() {
    let mut state = fresh_state();
    state.runtime.provider_capabilities.max_context_tokens = Some(200_000);
    state.session.append(ChatMessage::user("one"), state.now);
    state
        .session
        .append(ChatMessage::assistant("two"), state.now);
    state.session.context_usage = Some(crate::ContextUsageSnapshot::from_usage(
        &mermaid_model::models::TokenUsage::provider(120_000, 900),
        Some(200_000),
    ));
    state.session.cumulative_token_usage = TokenUsageTotals {
        prompt_tokens: 120_000,
        completion_tokens: 900,
        ..Default::default()
    };
    state.confirm = Some(crate::state::Confirmation {
        prompt: "Clear conversation history?".to_string(),
        accept_msg_token: crate::state::ConfirmationTarget::ClearConversation,
    });

    let (state, _) = update(state, Msg::ConfirmAccepted);

    assert!(state.session.messages().is_empty(), "history is wiped");
    let gauge = state
        .session
        .context_usage
        .as_ref()
        .expect("the gauge survives a clear");
    assert_eq!(gauge.max_tokens, Some(200_000), "the window carries over");
    assert!(
        gauge.used_tokens < 120_000,
        "clearing must shrink the context: {}",
        gauge.used_tokens
    );
    assert!(
        gauge.is_estimate(),
        "no provider has counted the fresh context yet"
    );
    // Cumulative spend is NOT context — those tokens were really spent, and
    // `/clear` is a new session for accounting.
    assert_eq!(state.session.cumulative_token_usage.prompt_tokens, 0);
    assert!(state.session.last_token_usage.is_none());
}

#[test]
fn confirm_declined_clears_without_action() {
    let mut state = fresh_state();
    state.session.append(ChatMessage::user("kept"), state.now);
    state.confirm = Some(crate::state::Confirmation {
        prompt: "Clear conversation history?".to_string(),
        accept_msg_token: crate::state::ConfirmationTarget::ClearConversation,
    });
    let (state, _) = update(state, Msg::ConfirmDeclined);
    assert_eq!(state.session.messages().len(), 1);
    assert!(state.confirm.is_none());
}

#[test]
fn mcp_server_ready_updates_entry_status() {
    let mut state = fresh_state();
    state.mcp = McpState::default();
    state.mcp.servers.insert(
        "s1".to_string(),
        McpServerEntry {
            config: crate::McpServerConfig {
                command: "echo".to_string(),
                args: vec![],
                env: std::collections::HashMap::new(),
                ..Default::default()
            },
            status: McpServerStatus::Starting,
            tools: vec![],
        },
    );
    let (state, _) = update(
        state,
        Msg::McpServerReady {
            name: "s1".to_string(),
            tools: vec![],
        },
    );
    assert_eq!(state.mcp.servers["s1"].status, McpServerStatus::Ready);
}

#[test]
fn build_chat_request_orders_mcp_tools_by_server_name() {
    // #F68: `state.mcp.servers` is a HashMap with per-process randomized
    // iteration order. `build_chat_request` must sort servers by name so the
    // emitted `ChatRequest.tools` ordering is deterministic across runs
    // (byte-reproducible requests / prompt-cache stability).
    let mut state = fresh_state();
    // Deferral would collapse the list to `tool_search`; this test pins
    // the DIRECT advertisement ordering, so turn deferral off.
    state.settings.mcp_defer_tools = Some(false);
    state.mcp = McpState::default();
    for name in ["zeta", "alpha", "mike", "bravo", "delta"] {
        state.mcp.servers.insert(
            name.to_string(),
            McpServerEntry {
                config: crate::McpServerConfig {
                    command: "echo".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    ..Default::default()
                },
                status: McpServerStatus::Ready,
                tools: vec![crate::state::McpToolSpec {
                    name: format!("mcp__{name}__do"),
                    raw_name: "do".to_string(),
                    description: "d".to_string(),
                    input_schema: serde_json::json!({}),
                    read_only_hint: false,
                }],
            },
        );
    }
    let request = build_chat_request(&state);
    // Scope to the MCP portion: the reducer also appends the mode-scoped
    // plan tool (enter/exit_plan_mode) after the MCP block.
    let names: Vec<&str> = request
        .tools
        .iter()
        .map(|t| t.name.as_str())
        .filter(|n| n.starts_with("mcp__"))
        .collect();
    assert_eq!(
        names,
        vec![
            "mcp__alpha__do",
            "mcp__bravo__do",
            "mcp__delta__do",
            "mcp__mike__do",
            "mcp__zeta__do",
        ],
        "MCP tools must be ordered by server name regardless of HashMap layout"
    );
}

#[test]
fn tool_search_call_is_intercepted_and_promotes_for_the_follow_up() {
    // A `tool_search` call never reaches the effect layer: the reducer
    // resolves it purely, promotes the matches, and the follow-up
    // CallModel already advertises the promoted tool directly.
    let mut state = fresh_state();
    state.mcp.servers.insert(
        "srv".to_string(),
        McpServerEntry {
            config: crate::McpServerConfig::default(),
            status: McpServerStatus::Ready,
            tools: vec![crate::state::McpToolSpec {
                name: "mcp__srv__alpha".to_string(),
                raw_name: "alpha".to_string(),
                description: "does alpha things".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
                read_only_hint: false,
            }],
        },
    );
    state.turn = TurnState::Generating {
        id: TurnId(5),
        started: std::time::SystemTime::now(),
        partial_text: String::new(),
        partial_reasoning: String::new(),
        tokens: 0,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: vec![mermaid_model::models::ToolCall {
            id: Some("call_ts".to_string()),
            function: mermaid_model::models::FunctionCall {
                name: crate::tool_search::TOOL_SEARCH_NAME.to_string(),
                arguments: serde_json::json!({"query": "alpha"}),
            },
        }],
        continuation: false,
    };
    let (state, cmds) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(5),
            usage: None,
            provider_continuation: None,
            stop_reason: None,
        },
    );
    assert!(
        !cmds.iter().any(|c| matches!(c, Cmd::ExecuteTool { .. })),
        "tool_search must not dispatch to the effect layer"
    );
    assert!(state.mcp.promoted.contains("mcp__srv__alpha"));
    let follow_up = cmds
        .iter()
        .find_map(|c| match c {
            Cmd::CallModel { request, .. } => Some(request),
            _ => None,
        })
        .expect("interception completes the batch and fires the follow-up");
    // Scope to the MCP portion: the reducer also appends the mode-scoped
    // plan tool after the MCP block.
    let names: Vec<&str> = follow_up
        .tools
        .iter()
        .map(|t| t.name.as_str())
        .filter(|n| n.starts_with("mcp__") || *n == "tool_search")
        .collect();
    assert_eq!(
        names,
        vec!["mcp__srv__alpha"],
        "promoted tool advertised directly; tool_search drops out once nothing is deferred"
    );
    // The tool result round-trips through the normal pairing machinery.
    let has_tool_result =
        state.session.messages().iter().any(|m| {
            m.role == mermaid_model::models::MessageRole::Tool && m.content.contains("alpha")
        });
    assert!(has_tool_result, "tool_search outcome committed to history");
}

#[test]
fn tool_search_with_deferral_off_returns_clean_no_op_outcome() {
    // A hallucinated tool_search while nothing is deferred must not
    // reach the effect layer's unknown-tool arm.
    let mut state = fresh_state();
    state.settings.mcp_defer_tools = Some(false);
    state.turn = TurnState::Generating {
        id: TurnId(6),
        started: std::time::SystemTime::now(),
        partial_text: String::new(),
        partial_reasoning: String::new(),
        tokens: 0,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: vec![mermaid_model::models::ToolCall {
            id: Some("call_ts2".to_string()),
            function: mermaid_model::models::FunctionCall {
                name: crate::tool_search::TOOL_SEARCH_NAME.to_string(),
                arguments: serde_json::json!({"query": "anything"}),
            },
        }],
        continuation: false,
    };
    let (state, cmds) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(6),
            usage: None,
            provider_continuation: None,
            stop_reason: None,
        },
    );
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::ExecuteTool { .. })));
    assert!(state.mcp.promoted.is_empty());
    let has_no_tools_note = state
        .session
        .messages()
        .iter()
        .any(|m| m.content.contains("No deferred MCP tools"));
    assert!(has_no_tools_note, "clean informative outcome committed");
}

fn pending_read_file_call() -> crate::state::PendingToolCall {
    crate::state::PendingToolCall {
        call_id: crate::ToolCallId(1),
        source: mermaid_model::models::ToolCall {
            id: Some("call_a".to_string()),
            function: mermaid_model::models::FunctionCall {
                name: "read_file".to_string(),
                arguments: serde_json::json!({}),
            },
        },
    }
}

#[test]
fn steering_delivers_all_queued_messages_at_the_tool_boundary() {
    let mut state = fresh_state();
    state
        .session
        .append(ChatMessage::assistant("calling a tool"), state.now);
    state.turn = crate::transition::start_executing_tools(
        TurnId(1),
        vec![pending_read_file_call()],
        std::time::SystemTime::now(),
    );
    for text in ["steer one", "steer two"] {
        state
            .ui
            .queued_messages
            .push_back(crate::state::QueuedMessage {
                text: text.to_string(),
                attachment_ids: vec![],
            });
    }
    let (state, cmds) = update(
        state,
        Msg::ToolFinished {
            turn: TurnId(1),
            call_id: crate::ToolCallId(1),
            outcome: ToolOutcome::success("file body", "read it", 0.1),
        },
    );
    assert!(state.ui.queued_messages.is_empty(), "queue fully drained");
    // Wire order: assistant → tool result → steered user texts, FIFO.
    let contents: Vec<&str> = state
        .session
        .messages()
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    let tool_pos = contents.iter().position(|c| c.contains("file body"));
    let one_pos = contents.iter().position(|c| *c == "steer one");
    let two_pos = contents.iter().position(|c| *c == "steer two");
    assert!(tool_pos < one_pos && one_pos < two_pos, "{contents:?}");
    // The follow-up request already carries the steered messages.
    let request = cmds
        .iter()
        .find_map(|c| match c {
            Cmd::CallModel { request, .. } => Some(request),
            _ => None,
        })
        .expect("follow-up CallModel");
    assert!(request.messages.iter().any(|m| m.content == "steer two"));
    // User-authored text persists at the boundary, not only at StreamDone.
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::SaveConversation { .. }))
    );
}

#[test]
fn steering_resolves_queued_image_tokens_against_owned_attachments() {
    let mut state = fresh_state();
    state
        .session
        .append(ChatMessage::assistant("calling a tool"), state.now);
    state.turn = crate::transition::start_executing_tools(
        TurnId(1),
        vec![pending_read_file_call()],
        std::time::SystemTime::now(),
    );
    state.ui.attachments.push(crate::state::Attachment {
        id: 7,
        number: 1,
        base64_data: "aGk=".to_string(),
        temp_path: std::path::PathBuf::from("/tmp/x.png"),
        size_bytes: 2,
        format: "png".to_string(),
    });
    state
        .ui
        .queued_messages
        .push_back(crate::state::QueuedMessage {
            text: "look at [Image #1]".to_string(),
            attachment_ids: vec![7],
        });
    let (state, _) = update(
        state,
        Msg::ToolFinished {
            turn: TurnId(1),
            call_id: crate::ToolCallId(1),
            outcome: ToolOutcome::success("done", "done", 0.1),
        },
    );
    let steered = state
        .session
        .messages()
        .iter()
        .find(|m| m.content.contains("[Image #1]"))
        .expect("steered message committed");
    assert_eq!(steered.images.as_ref().map(Vec::len), Some(1));
    assert!(state.ui.attachments.is_empty(), "attachment consumed");
}

#[test]
fn execute_tool_cmd_carries_the_session_anchor() {
    let mut state = state_with_two_exchanges();
    let expected_session = state.session.conversation.id.clone();
    let expected_scratchpad = std::path::PathBuf::from("/data/tmp/scratchpad/-proj/s");
    state.session.scratchpad = Some(expected_scratchpad.clone());
    state.turn = TurnState::Generating {
        id: TurnId(9),
        started: std::time::SystemTime::now(),
        partial_text: String::new(),
        partial_reasoning: String::new(),
        tokens: 0,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: vec![mermaid_model::models::ToolCall {
            id: Some("call_b".to_string()),
            function: mermaid_model::models::FunctionCall {
                name: "read_file".to_string(),
                arguments: serde_json::json!({}),
            },
        }],
        continuation: false,
    };
    let (state, cmds) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(9),
            usage: None,
            provider_continuation: None,
            stop_reason: None,
        },
    );
    let (session_id, message_index, scratchpad) = cmds
        .iter()
        .find_map(|c| match c {
            Cmd::ExecuteTool { dispatch, .. } => Some((
                dispatch.session_id.clone(),
                dispatch.message_index,
                dispatch.scratchpad.clone(),
            )),
            _ => None,
        })
        .expect("ExecuteTool dispatched");
    assert_eq!(session_id, expected_session);
    assert_eq!(
        scratchpad.as_deref(),
        Some(expected_scratchpad.as_path()),
        "the materialized scratch dir rides on the dispatch"
    );
    assert_eq!(
        message_index,
        state.session.messages().len(),
        "stamped at dispatch, after the assistant tool_use commit"
    );
}

/// Alt+P is gone: plan is a position in the Shift+Tab cycle, so the chord
/// that used to toggle it must not do anything special any more.
#[test]
fn alt_p_no_longer_toggles_plan_mode() {
    let (state, _) = update(
        fresh_state(),
        Msg::Key(Key {
            code: KeyCode::Char('p'),
            modifiers: KeyMods {
                alt: true,
                ..Default::default()
            },
        }),
    );
    assert!(state.session.plan.is_none());
    assert_eq!(state.session.safety_mode, Config::default().safety.mode);
}

/// Shift+Tab walks the whole cycle including plan, and entering plan that
/// way allocates the plan file exactly as `/plan` does.
#[test]
fn shift_tab_cycles_into_plan_mode_and_allocates_a_plan_path() {
    use mermaid_model::safety::SafetyMode as S;
    let tab = || {
        Msg::Key(Key {
            code: KeyCode::BackTab,
            modifiers: KeyMods::default(),
        })
    };
    // ask -> auto -> full_access -> plan
    let (state, _) = update(fresh_state(), tab());
    assert_eq!(state.session.safety_mode, S::Auto);
    let (state, _) = update(state, tab());
    assert_eq!(state.session.safety_mode, S::FullAccess);
    let (state, _) = update(state, tab());
    assert_eq!(state.session.safety_mode, S::Plan);

    let plan = state
        .session
        .plan
        .clone()
        .expect("cycling into plan allocates the plan data");
    assert!(
        plan.plan_path.starts_with("/tmp/project/.mermaid/plans"),
        "plan file is project-local: {:?}",
        plan.plan_path
    );
    assert!(plan.plan_path.extension().is_some_and(|e| e == "md"));
    // The status band announces the mode — cycling adds no transcript row.
    assert!(
        state.session.messages().is_empty(),
        "entry adds no transcript row (status band carries the mode): {:?}",
        state.session.messages()
    );

    // One more Shift+Tab leaves plan for the next mode in the cycle — no
    // remembered restore target, just the next position.
    let (state, _) = update(state, tab());
    assert_eq!(state.session.safety_mode, S::ReadOnly);
    assert!(state.session.plan.is_none(), "plan data is torn down");
    assert!(
        state.session.messages().is_empty(),
        "exit adds no transcript row either: {:?}",
        state.session.messages()
    );
}

#[test]
fn slash_plan_enters_shows_and_leaves() {
    let (state, _) = update(fresh_state(), Msg::Slash(SlashCmd::Plan(None)));
    assert!(state.session.plan.is_some());
    let (state, _) = update(state, Msg::Slash(SlashCmd::Plan(Some("show".to_string()))));
    assert!(
        state
            .session
            .messages()
            .iter()
            .any(|m| m.content.contains("Plan file (drafting):")),
        "/plan show prints the path"
    );
    let (state, _) = update(state, Msg::Slash(SlashCmd::Plan(Some("off".to_string()))));
    assert!(state.session.plan.is_none());
}

#[test]
fn plan_path_allocation_is_deterministic() {
    // `--replay` must allocate the identical path: no wall clock, no RNG.
    let state = fresh_state();
    assert_eq!(plan_path_for(&state), plan_path_for(&state));
}

/// While planning, the base prompt must stop recommending `task_create` and
/// demanding implementation — the plan appendix owns behavior, and weak
/// models resolve contradictions by momentum.
#[test]
fn system_prompt_drops_task_create_advice_while_planning() {
    let mut state = fresh_state();
    let normal = system_prompt_for_state(&state);
    assert!(normal.contains("FULL initial plan in one call"));
    assert!(normal.contains("Do not stop at a proposal"));
    enter_planning(&mut state, "/tmp/project/.mermaid/plans/x.md");
    let planning = system_prompt_for_state(&state);
    assert!(!planning.contains("FULL initial plan in one call"));
    assert!(!planning.contains("Do not stop at a proposal"));
    assert!(
        planning.contains("## Plan Mode"),
        "the appendix still lands after the swap"
    );
}

// ── Context-delta injector ───────────────────────────────────────

use mermaid_model::models::ChatMessageKind;

/// Toggle plan mode the way a user does now that Alt+P is gone.
fn plan_on() -> Msg {
    Msg::Slash(SlashCmd::Plan(None))
}
fn plan_off() -> Msg {
    Msg::Slash(SlashCmd::Plan(Some("off".to_string())))
}

/// Dispatch once and return the request the model would see.
fn dispatch(state: &mut State, turn: u64) -> ChatRequest {
    let mut cmds = Vec::new();
    super::push_call_model(state, &mut cmds, TurnId(turn));
    match cmds.into_iter().find_map(|c| match c {
        Cmd::CallModel { request, .. } => Some(request),
        _ => None,
    }) {
        Some(request) => request,
        None => panic!("push_call_model must emit CallModel"),
    }
}

/// Put a session into plan mode the way the reducer does: the MODE becomes
/// `Plan` and `session.plan` carries only the plan's data. Setting the data
/// without the mode is the state this refactor made unrepresentable, so
/// tests must not hand-roll it.
fn enter_planning(state: &mut State, plan_path: &str) {
    state.session.safety_mode = mermaid_model::safety::SafetyMode::Plan;
    state.session.plan = Some(crate::PlanState {
        plan_path: PathBuf::from(plan_path),
        ..Default::default()
    });
}

/// Leave plan mode the way the reducer does: clear BOTH the mode and the
/// data. Clearing only `session.plan` leaves `safety_mode == Plan`, which
/// is the same half-state `enter_planning` guards against.
fn exit_planning(state: &mut State) {
    state.session.plan = None;
    state.session.safety_mode = mermaid_model::safety::SafetyMode::default();
}

fn markers(request: &ChatRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|m| m.kind == ChatMessageKind::ContextMarker)
        .map(|m| m.content.clone())
        .collect()
}

/// Issue #282: plan mode and safety mode used to be orthogonal values, so
/// Shift+Tab while planning set `full_access` live and the injector emitted
/// a permanent, never-swept "Safety mode changed … to `full_access`" marker
/// while the plan read-only floor was still in force. The model read that
/// as permission to mutate and collected denials — the exact loop the
/// salience work exists to prevent.
///
/// With one mode value the contradiction is unrepresentable: Shift+Tab
/// moves the LIVE mode, and the plan floor exists only while that mode is
/// `Plan`. So no marker can ever announce a mode the floor contradicts —
/// leaving plan and dropping the floor are the same event.
#[test]
fn shift_tab_out_of_plan_moves_the_live_mode_and_drops_the_floor_together() {
    use mermaid_model::safety::SafetyMode;
    let mut state = fresh_state();
    dispatch(&mut state, 1);
    let (mut state, _) = update(state, Msg::Slash(SlashCmd::Plan(None)));
    assert_eq!(state.session.safety_mode, SafetyMode::Plan);

    // While the mode IS plan, no marker may claim otherwise.
    let request = dispatch(&mut state, 2);
    for marker in markers(&request) {
        assert!(
            !marker.contains("Safety mode changed"),
            "a safety-delta marker would contradict the plan floor: {marker}",
        );
    }

    // Shift+Tab leaves plan for the next cycle position, and the plan data
    // goes with it — the floor cannot outlive the mode.
    let (state, _) = update(
        state,
        Msg::Key(Key {
            code: KeyCode::BackTab,
            modifiers: KeyMods::default(),
        }),
    );
    assert_eq!(state.session.safety_mode, SafetyMode::ReadOnly);
    assert!(
        state.session.plan.is_none(),
        "plan data must not survive the mode that defines it",
    );
}

/// The staleness nudge named `task_update` while plan mode withdrew it, so
/// the model was told to call a tool it was never shown and the gate
/// hard-errors. Only a successful update resets the counter, so the
/// contradiction re-injected itself every `TASK_STALENESS_CALLS` dispatches.
#[test]
fn no_task_staleness_nudge_while_the_checklist_writers_are_withdrawn() {
    use crate::ChecklistStatus::InProgress;
    let mut state = fresh_state();
    state.session.conversation.tasks = sample_task_store(&[InProgress]);
    enter_planning(&mut state, "/tmp/project/.mermaid/plans/x.md");

    for turn in 1..=(TASK_STALENESS_CALLS as u64 + 2) {
        let request = dispatch(&mut state, turn);
        let text = format!(
            "{}{}",
            request.instructions.clone().unwrap_or_default(),
            request
                .messages
                .iter()
                .map(|m| m.content.clone())
                .collect::<String>()
        );
        assert!(
            !text.contains("without a checklist update"),
            "planning must not nudge toward a withdrawn tool (turn {turn})",
        );
    }
    assert_eq!(
        state.runtime.calls_since_task_update, 0,
        "the counter stays parked so the next run does not inherit a primed nudge",
    );
}

/// The plan-mode section splice runs to the next `\n## ` heading, or to
/// end-of-string when there is none. Applied to the RENDERED prompt, a
/// base whose last section is `## Task Planning` meant the splice ate the
/// user's `append_system_prompt` entries too — silently dropping standing
/// instructions on every request while planning.
#[test]
fn plan_adaptation_preserves_append_system_prompt_extras() {
    let mut state = fresh_state();
    state.settings.prompt.system_prompt = Some(
        "# House rules\n\nBe brief.\n\n## Task Planning\n\nUse task_create for the FULL \
             initial plan."
            .to_string(),
    );
    state.settings.prompt.append_system_prompt = vec!["Never touch db/migrations/.".to_string()];
    enter_planning(&mut state, "/tmp/project/.mermaid/plans/x.md");

    let prompt = crate::request::system_prompt_for_state(&state);
    assert!(
        prompt.contains("Never touch db/migrations/."),
        "the user's appended instruction must survive the splice:\n{prompt}",
    );
    assert!(
        prompt.contains("disabled while a plan is being drafted"),
        "the section is still adapted for plan mode",
    );
    assert!(
        !prompt.contains("FULL initial plan"),
        "the contradicting execution imperative is still removed",
    );
}

#[test]
fn plan_entry_is_announced_at_the_next_dispatch_not_the_keypress() {
    let mut state = fresh_state();
    // First dispatch establishes the baseline silently.
    let req = dispatch(&mut state, 1);
    assert!(markers(&req).is_empty(), "baseline seed must be silent");
    // The keypress itself appends nothing (status band carries the mode).
    let (mut state, _) = update(state, plan_on());
    assert!(
        state
            .session
            .messages()
            .iter()
            .all(|m| m.kind != ChatMessageKind::ContextMarker)
    );
    // The next dispatch carries exactly one marker teaching the essentials.
    let req = dispatch(&mut state, 2);
    let m = markers(&req);
    assert_eq!(m.len(), 1, "exactly one marker: {m:?}");
    assert!(m[0].contains("Plan mode is now ON"));
    // The marker must name the REAL plan path. Asserted against the path
    // from state, not a hard-coded `.mermaid/plans` literal: the marker
    // renders it with `Path::display()`, which is backslash-separated on
    // Windows, and a forward-slash substring silently only ever held on
    // unix.
    let plan_path = state
        .session
        .plan
        .as_ref()
        .expect("planning")
        .plan_path
        .display()
        .to_string();
    assert!(
        m[0].contains(&plan_path),
        "marker must name the plan path {plan_path}: {}",
        m[0],
    );
    assert!(m[0].contains("write_file"));
    assert!(m[0].contains("exit_plan_mode"));
    // No change → no new marker.
    let req = dispatch(&mut state, 3);
    assert_eq!(markers(&req).len(), 1, "unchanged context injects nothing");
}

#[test]
fn context_marker_coalesces_plan_entry_with_the_plan_model_swap() {
    let mut state = fresh_state();
    state.settings.plan.model = Some("ollama/plan-brain".to_string());
    dispatch(&mut state, 1);
    let (mut state, _) = update(state, plan_on());
    let req = dispatch(&mut state, 2);
    let m = markers(&req);
    assert_eq!(m.len(), 1, "one coalesced marker, not two: {m:?}");
    assert!(m[0].contains("Plan mode is now ON"));
    assert!(m[0].contains("ollama/plan-brain"));
}

#[test]
fn plan_exit_marker_is_unconditional_and_steering_is_denial_gated() {
    // Without denials: the exit is announced, the re-attempt sentence not.
    let mut state = fresh_state();
    dispatch(&mut state, 1);
    let (state, _) = update(state, plan_on());
    let mut state = state;
    dispatch(&mut state, 2);
    let (mut state, _) = update(state, plan_off());
    let req = dispatch(&mut state, 3);
    let m = markers(&req);
    let exit = m.last().expect("exit marker");
    assert!(exit.contains("Plan mode is now OFF"));
    assert!(!exit.contains("re-attempt gated actions"));

    // With a standing plan denial: the steering sentence rides the marker.
    let mut state = fresh_state();
    dispatch(&mut state, 1);
    let (mut state, _) = update(state, plan_on());
    dispatch(&mut state, 2);
    state.session.append(
        ChatMessage::assistant("editing").with_tool_calls(vec![
            mermaid_model::models::tool_call::ToolCall {
                id: Some("call-1".to_string()),
                function: mermaid_model::models::tool_call::FunctionCall {
                    name: "write_file".to_string(),
                    arguments: serde_json::json!({}),
                },
            },
        ]),
        state.now,
    );
    state.session.append(
        ChatMessage::tool(
            "call-1",
            "write_file",
            format!(
                "write_file(x) blocked by policy: {} is active",
                mermaid_model::safety::PLAN_DENIAL_MARKER
            ),
        ),
        state.now,
    );
    let (mut state, _) = update(state, plan_off());
    let req = dispatch(&mut state, 3);
    let m = markers(&req);
    let exit = m.last().expect("exit marker");
    assert!(exit.contains("Plan mode is now OFF"));
    assert!(
        exit.contains("re-attempt gated actions"),
        "denials in history must add the steering sentence: {exit}"
    );
}

#[test]
fn rapid_plan_toggle_between_dispatches_injects_nothing() {
    let mut state = fresh_state();
    dispatch(&mut state, 1);
    let (state, _) = update(state, plan_on());
    let (mut state, _) = update(state, plan_off());
    let req = dispatch(&mut state, 2);
    assert!(
        markers(&req).is_empty(),
        "on→off between dispatches collapses to no marker"
    );
}

#[test]
fn safety_mode_flip_is_announced_exactly_once() {
    use mermaid_model::safety::SafetyMode;
    let mut state = fresh_state();
    dispatch(&mut state, 1);
    state.session.safety_mode = SafetyMode::ReadOnly;
    let req = dispatch(&mut state, 2);
    let m = markers(&req);
    assert_eq!(m.len(), 1);
    assert!(m[0].contains("Safety mode changed"));
    assert!(m[0].contains("read_only"));
    let req = dispatch(&mut state, 3);
    assert_eq!(markers(&req).len(), 1, "no re-announcement");
}

#[test]
fn first_dispatch_of_a_pre_field_save_stamps_silently() {
    // A resumed mid-plan save from before the field existed: plan is
    // Some, baseline is None. Announce nothing; the appendix + reminder
    // cover it.
    let mut state = fresh_state();
    enter_planning(&mut state, "/tmp/project/.mermaid/plans/x.md");
    assert!(state.session.conversation.advertised_context.is_none());
    let req = dispatch(&mut state, 1);
    assert!(markers(&req).is_empty());
    let snap = state
        .session
        .conversation
        .advertised_context
        .as_ref()
        .expect("baseline stamped");
    assert!(snap.plan_path.is_some(), "snapshot records live plan state");
}

#[test]
fn subagents_get_no_markers_or_reminders() {
    use mermaid_model::safety::SafetyMode;
    let mut state = fresh_state();
    state.session.is_subagent = true;
    dispatch(&mut state, 1);
    state.session.safety_mode = SafetyMode::FullAccess;
    let req = dispatch(&mut state, 2);
    assert!(markers(&req).is_empty(), "subagents get no markers");
    assert_eq!(
        state
            .session
            .conversation
            .advertised_context
            .as_ref()
            .map(|c| c.safety_mode),
        Some(SafetyMode::FullAccess),
        "snapshot still refreshes silently"
    );
    assert!(
        req.messages
            .iter()
            .all(|m| !m.content.starts_with(PLAN_REMINDER_PREFIX)),
        "subagents get no plan reminders"
    );
}

#[test]
fn plan_reminder_rides_every_dispatch_and_never_duplicates() {
    let mut state = fresh_state();
    dispatch(&mut state, 1);
    let (mut state, _) = update(state, plan_on());
    for turn in 2..4 {
        let req = dispatch(&mut state, turn);
        let reminders: Vec<_> = req
            .messages
            .iter()
            .filter(|m| m.content.starts_with(PLAN_REMINDER_PREFIX))
            .collect();
        assert_eq!(reminders.len(), 1, "exactly one reminder per request");
        assert_eq!(reminders[0].kind, ChatMessageKind::RecoveryNudge);
        assert!(reminders[0].content.contains("write_file"));
        assert!(
            req.messages
                .last()
                .is_some_and(|m| m.content.starts_with(PLAN_REMINDER_PREFIX)),
            "the reminder sits at the history tail"
        );
    }
}

#[test]
fn plan_reminder_is_retracted_when_plan_mode_exits() {
    let mut state = fresh_state();
    dispatch(&mut state, 1);
    let (mut state, _) = update(state, plan_on());
    dispatch(&mut state, 2);
    assert!(
        state
            .session
            .messages()
            .iter()
            .any(|m| m.content.starts_with(PLAN_REMINDER_PREFIX))
    );
    let (mut state, _) = update(state, plan_off());
    assert!(
        state
            .session
            .messages()
            .iter()
            .all(|m| !m.content.starts_with(PLAN_REMINDER_PREFIX)),
        "leaving plan mode retracts the standing reminder"
    );
    let req = dispatch(&mut state, 3);
    assert!(
        req.messages
            .iter()
            .all(|m| !m.content.starts_with(PLAN_REMINDER_PREFIX))
    );
}

#[test]
fn context_markers_survive_the_turn_end_sweep() {
    let mut state = fresh_state();
    dispatch(&mut state, 1);
    let (mut state, _) = update(state, plan_on());
    dispatch(&mut state, 2);
    assert!(
        state
            .session
            .messages()
            .iter()
            .any(|m| m.kind == ChatMessageKind::ContextMarker)
    );
    sweep_spent_nudges(&mut state);
    assert!(
        state
            .session
            .messages()
            .iter()
            .any(|m| m.kind == ChatMessageKind::ContextMarker),
        "markers are the durable timeline record"
    );
    assert!(
        state
            .session
            .messages()
            .iter()
            .all(|m| !m.content.starts_with(PLAN_REMINDER_PREFIX)),
        "the reminder is swept like any RecoveryNudge"
    );
}

// ── Plan doom-loop breaker ───────────────────────────────────────

fn plan_denial_outcome() -> ToolOutcome {
    ToolOutcome::error(
        format!(
            "execute_command echo x > src/a.rs blocked by policy: {} is active — planning \
                 only",
            mermaid_model::safety::PLAN_DENIAL_MARKER
        ),
        0.0,
    )
}

/// A successful call that the GATE approved via the plan-file carve-out —
/// the fact the breaker disarms on, whatever tool produced it.
fn plan_file_write_outcome() -> ToolOutcome {
    let mut outcome = ToolOutcome::success("wrote the plan", "wrote", 0.0);
    outcome.metadata.plan_file_written = true;
    outcome
}

/// The escalated corrective tells the model "a shell redirect writing ONLY
/// that file works too". When the model complied, the old tool-name disarm
/// (`write_file`/`apply_patch` only) never fired, so the breaker stayed
/// armed and kept re-injecting "the plan file does not exist until you
/// write it" — false, and it steered the model into rewriting a plan it
/// had already authored instead of calling `exit_plan_mode`.
#[test]
fn a_shell_redirect_plan_write_disarms_the_stall_breaker() {
    let mut state = fresh_state();
    dispatch(&mut state, 1);
    let (mut state, _) = update(state, plan_on());
    note_plan_tool_outcome(
        &mut state.runtime,
        true,
        "execute_command",
        &plan_denial_outcome(),
    );
    assert!(state.runtime.plan_thrash_armed, "a denied mutation arms it");
    state.runtime.plan_calls_since_denial = 2;

    note_plan_tool_outcome(
        &mut state.runtime,
        true,
        "execute_command",
        &plan_file_write_outcome(),
    );
    assert!(
        !state.runtime.plan_thrash_armed,
        "the shell spelling of plan authoring disarms the breaker too",
    );
    assert_eq!(state.runtime.plan_calls_since_denial, 0);
}

/// The breaker's doc contract says a read-heavy Ground phase must never
/// trip it. Arming on ANY denial carrying the plan marker broke that: with
/// `[plan] web = deny`, `web_fetch` denials armed it and three dispatches
/// later the model was told to "STOP attempting other mutations" and write
/// the plan NOW — cutting research short on a purely read-only phase.
#[test]
fn non_mutating_plan_denials_do_not_arm_the_stall_breaker() {
    let mut state = fresh_state();
    dispatch(&mut state, 1);
    let (mut state, _) = update(state, plan_on());
    for tool in ["web_fetch", "web_search", "memory", "task_create"] {
        note_plan_tool_outcome(&mut state.runtime, true, tool, &plan_denial_outcome());
        assert!(
            !state.runtime.plan_thrash_armed,
            "a denied {tool} is not a mutation attempt",
        );
    }
    // A denied mutation still arms it.
    note_plan_tool_outcome(
        &mut state.runtime,
        true,
        "write_file",
        &plan_denial_outcome(),
    );
    assert!(state.runtime.plan_thrash_armed);
}

#[test]
fn plan_stall_escalates_the_reminder_after_repeated_denials() {
    let mut state = fresh_state();
    dispatch(&mut state, 1);
    let (mut state, _) = update(state, plan_on());
    // Pure exploration never escalates, however long it runs.
    for turn in 2..8 {
        let req = dispatch(&mut state, turn);
        let reminder = req.messages.last().expect("reminder at tail");
        assert!(
            !reminder.content.contains("STOP attempting"),
            "unarmed breaker must not escalate (turn {turn})"
        );
    }
    // A plan denial arms it...
    note_plan_tool_outcome(
        &mut state.runtime,
        true,
        "execute_command",
        &plan_denial_outcome(),
    );
    assert!(state.runtime.plan_thrash_armed);
    // ...and PLAN_THRASH_CALLS dispatches later the reminder escalates.
    for turn in 8..8 + u64::from(PLAN_THRASH_CALLS) - 1 {
        let req = dispatch(&mut state, turn);
        assert!(
            !req.messages
                .last()
                .is_some_and(|m| m.content.contains("STOP attempting")),
            "not yet (turn {turn})"
        );
    }
    let req = dispatch(&mut state, 20);
    let reminder = req.messages.last().expect("reminder at tail");
    assert!(
        reminder.content.contains("STOP attempting"),
        "threshold dispatch carries the corrective: {}",
        reminder.content
    );
    assert!(reminder.content.contains("write_file"));
    assert_eq!(
        state.runtime.plan_calls_since_denial, 0,
        "re-armed after firing"
    );
}

#[test]
fn plan_write_disarms_the_stall_breaker() {
    let mut state = fresh_state();
    dispatch(&mut state, 1);
    let (mut state, _) = update(state, plan_on());
    note_plan_tool_outcome(
        &mut state.runtime,
        true,
        "execute_command",
        &plan_denial_outcome(),
    );
    state.runtime.plan_calls_since_denial = 2;
    // A successful write that was NOT the plan file (e.g. a memory write
    // the plan profile allows) must NOT disarm — the tool name alone was
    // never evidence a plan got written.
    note_plan_tool_outcome(
        &mut state.runtime,
        true,
        "write_file",
        &ToolOutcome::success("wrote memory", "wrote", 0.0),
    );
    assert!(
        state.runtime.plan_thrash_armed,
        "a non-plan write must leave the breaker armed",
    );
    note_plan_tool_outcome(
        &mut state.runtime,
        true,
        "write_file",
        &plan_file_write_outcome(),
    );
    assert!(!state.runtime.plan_thrash_armed, "a plan write disarms");
    assert_eq!(state.runtime.plan_calls_since_denial, 0);
    // Outside plan mode the bookkeeping is inert.
    note_plan_tool_outcome(
        &mut state.runtime,
        false,
        "execute_command",
        &plan_denial_outcome(),
    );
    assert!(!state.runtime.plan_thrash_armed);
    // Exiting plan mode clears an armed breaker.
    state.runtime.plan_thrash_armed = true;
    state.runtime.plan_calls_since_denial = 1;
    let (state, _) = update(state, plan_off());
    assert!(!state.runtime.plan_thrash_armed, "exit clears the breaker");
    assert_eq!(state.runtime.plan_calls_since_denial, 0);
}

#[test]
fn forked_handoff_announces_plan_end_in_the_new_conversation() {
    let mut state = fresh_state();
    dispatch(&mut state, 1);
    let (mut state, _) = update(state, plan_on());
    dispatch(&mut state, 2);
    let old_id = state.session.conversation.id.clone();
    let mut cmds = Vec::new();
    super::handoff_plan_mode(
        &mut state,
        &mut cmds,
        "## Tasks\n1. Step one\n",
        false,
        true,
        None,
    );
    assert_ne!(state.session.conversation.id, old_id, "forked");
    assert!(
        state.session.conversation.advertised_context.is_some(),
        "fork inherits the baseline"
    );
    let req = dispatch(&mut state, 3);
    let m = markers(&req);
    assert!(
        m.iter().any(|c| c.contains("Plan mode is now OFF")),
        "the NEW conversation's first dispatch announces the exit: {m:?}"
    );
}

#[test]
fn plan_mode_floors_dispatch_to_read_only_and_stamps_the_plan_file() {
    use mermaid_model::safety::SafetyMode;
    let mut state = state_with_two_exchanges();
    // Entering plan mode from full_access: the MODE becomes plan, and
    // full_access is staged as the resume target.
    state.session.safety_mode = SafetyMode::FullAccess;
    let plan_path = PathBuf::from("/tmp/project/.mermaid/plans/x.md");
    enter_planning(&mut state, &plan_path.display().to_string());
    state.turn = TurnState::Generating {
        id: TurnId(9),
        started: std::time::SystemTime::now(),
        partial_text: String::new(),
        partial_reasoning: String::new(),
        tokens: 0,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: vec![mermaid_model::models::ToolCall {
            id: Some("call_b".to_string()),
            function: mermaid_model::models::FunctionCall {
                name: "write_file".to_string(),
                arguments: serde_json::json!({}),
            },
        }],
        continuation: false,
    };
    let (_state, cmds) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(9),
            usage: None,
            provider_continuation: None,
            stop_reason: None,
        },
    );
    let (mode, plan_file) = cmds
        .iter()
        .find_map(|c| match c {
            Cmd::ExecuteTool { dispatch, .. } => {
                Some((dispatch.safety_mode, dispatch.plan_file.clone()))
            },
            _ => None,
        })
        .expect("ExecuteTool dispatched");
    // The dispatched mode IS `Plan` — it carries the read-only floor in
    // the policy engine itself, so nothing has to substitute `ReadOnly`
    // for it here. Entering from full_access does not leak that mode.
    assert_eq!(
        mode,
        SafetyMode::Plan,
        "plan mode dispatches as Plan, not as the pre-plan mode"
    );
    assert!(mode.is_planning());
    assert_eq!(plan_file, Some(plan_path));
}

#[test]
fn build_chat_request_neutralizes_a_superseded_plan_denial() {
    // Once plan mode ends its denials stop describing the live policy —
    // the wire history must stop asserting them.
    let mut state = fresh_state();
    let call = mermaid_model::models::tool_call::ToolCall {
        id: Some("call-1".to_string()),
        function: mermaid_model::models::tool_call::FunctionCall {
            name: "write_file".to_string(),
            arguments: serde_json::json!({ "path": "main.qml" }),
        },
    };
    state.session.append(
        ChatMessage::assistant("editing").with_tool_calls(vec![call]),
        state.now,
    );
    state.session.append(
        ChatMessage::tool(
            "call-1",
            "write_file",
            format!(
                "write_file(main.qml) blocked by policy: {} is active — planning only",
                mermaid_model::safety::PLAN_DENIAL_MARKER
            ),
        ),
        state.now,
    );

    // While planning: the denial stands verbatim.
    enter_planning(&mut state, "/tmp/project/.mermaid/plans/x.md");
    let req = build_chat_request(&state);
    let tool_msg = req
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .expect("tool_result in request");
    assert!(
        tool_msg.content.contains("blocked by policy"),
        "denial must stand while planning: {:?}",
        tool_msg.content
    );

    // Plan mode off: rewritten to a past-tense note.
    exit_planning(&mut state);
    let req = build_chat_request(&state);
    let tool_msg = req
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .expect("tool_result in request");
    assert!(
        !tool_msg.content.contains("blocked by policy"),
        "stale plan denial must be neutralized: {:?}",
        tool_msg.content
    );
    assert!(tool_msg.content.contains("no longer applies"));
}

#[test]
fn plan_mode_keeps_read_only_denials_standing() {
    use mermaid_model::safety::SafetyMode;
    // full_access + planning: the EFFECTIVE mode is the read-only floor,
    // so a pre-plan read-only denial still describes reality — the
    // loosened-mode rewrite must not fire.
    let mut state = fresh_state();
    state.session.safety_mode = SafetyMode::FullAccess;
    enter_planning(&mut state, "/tmp/project/.mermaid/plans/x.md");
    let call = mermaid_model::models::tool_call::ToolCall {
        id: Some("call-1".to_string()),
        function: mermaid_model::models::tool_call::FunctionCall {
            name: "write_file".to_string(),
            arguments: serde_json::json!({ "path": "main.qml" }),
        },
    };
    state.session.append(
        ChatMessage::assistant("editing").with_tool_calls(vec![call]),
        state.now,
    );
    state.session.append(
        ChatMessage::tool(
            "call-1",
            "write_file",
            format!(
                "write_file(main.qml) blocked by policy: {} blocks mutations and control actions",
                mermaid_model::safety::READ_ONLY_DENIAL_MARKER
            ),
        ),
        state.now,
    );
    let req = build_chat_request(&state);
    let tool_msg = req
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .expect("tool_result in request");
    assert!(
        tool_msg.content.contains("blocked by policy"),
        "read-only denial stands while the plan floor applies: {:?}",
        tool_msg.content
    );
}

#[test]
fn system_prompt_names_the_scratchpad_path_once_ready() {
    let mut state = fresh_state();
    assert!(
        !system_prompt_for_state(&state).contains("Scratchpad directory:"),
        "no path line before ScratchpadReady lands"
    );
    state.session.scratchpad = Some(PathBuf::from("/tmp/mermaid-1000/-proj/s/scratchpad"));
    let prompt = system_prompt_for_state(&state);
    assert!(
        prompt.contains("Scratchpad directory: /tmp/mermaid-1000/-proj/s/scratchpad"),
        "the session block names the concrete scratchpad: {prompt}"
    );
}

#[test]
fn system_prompt_carries_the_plan_block_only_while_planning() {
    let mut state = fresh_state();
    let prompt = system_prompt_for_state(&state);
    assert!(!prompt.contains("## Plan Mode"));
    enter_planning(&mut state, "/tmp/project/.mermaid/plans/x.md");
    let prompt = system_prompt_for_state(&state);
    assert!(prompt.contains("## Plan Mode"));
    assert!(
        prompt.contains("/tmp/project/.mermaid/plans/x.md"),
        "the plan block names the concrete plan file"
    );
    assert!(
        prompt.contains("Safety mode: plan"),
        "the session block must not invite gated actions while planning"
    );
}

#[test]
fn build_chat_request_advertises_the_right_plan_tool() {
    let mut state = fresh_state();
    let names = |state: &State| -> Vec<String> {
        build_chat_request(state)
            .tools
            .iter()
            .map(|t| t.name.clone())
            .collect()
    };
    // Not planning: enter_plan_mode only.
    let n = names(&state);
    assert!(n.contains(&"enter_plan_mode".to_string()));
    assert!(!n.contains(&"exit_plan_mode".to_string()));
    // Planning: exit_plan_mode only.
    enter_planning(&mut state, "/tmp/project/.mermaid/plans/x.md");
    let n = names(&state);
    assert!(n.contains(&"exit_plan_mode".to_string()));
    assert!(!n.contains(&"enter_plan_mode".to_string()));
    // Subagents get neither.
    exit_planning(&mut state);
    state.session.is_subagent = true;
    let n = names(&state);
    assert!(!n.contains(&"enter_plan_mode".to_string()));
    assert!(!n.contains(&"exit_plan_mode".to_string()));
}

/// Plan mode must stop ADVERTISING the checklist writers — their schema
/// descriptions recommend exactly the call the gate then hard-errors.
#[test]
fn build_chat_request_suppresses_task_writers_while_planning() {
    let mut state = fresh_state();
    assert!(
        build_chat_request(&state)
            .suppressed_builtin_tools
            .is_empty(),
        "nothing suppressed outside plan mode"
    );
    enter_planning(&mut state, "/tmp/project/.mermaid/plans/x.md");
    assert_eq!(
        build_chat_request(&state).suppressed_builtin_tools,
        vec!["task_create", "task_update"],
        "planning hides the writers, keeps task_list"
    );
    // An explicit `tasks = allow` in the plan profile restores them,
    // matching the runtime backstop in tasks::plan_mode_block.
    state.settings.plan.permissions.tasks = crate::PlanPermLevel::Allow;
    assert!(
        build_chat_request(&state)
            .suppressed_builtin_tools
            .is_empty(),
        "tasks=allow restores advertisement"
    );
    // Subagents never plan, so nothing is suppressed for them either.
    state.settings.plan.permissions.tasks = crate::PlanPermLevel::Deny;
    exit_planning(&mut state);
    state.session.is_subagent = true;
    assert!(
        build_chat_request(&state)
            .suppressed_builtin_tools
            .is_empty()
    );
}

/// Drive a single named tool call through `StreamDone` so the turn lands in
/// `ExecutingTools`, returning the allocated call id.
fn drive_single_tool_call(state: &mut State, tool: &str) -> crate::ToolCallId {
    state.turn = TurnState::Generating {
        id: TurnId(9),
        started: std::time::SystemTime::now(),
        partial_text: String::new(),
        partial_reasoning: String::new(),
        tokens: 0,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: vec![mermaid_model::models::ToolCall {
            id: Some("call_p".to_string()),
            function: mermaid_model::models::FunctionCall {
                name: tool.to_string(),
                arguments: serde_json::json!({}),
            },
        }],
        continuation: false,
    };
    let (next, cmds) = update(
        std::mem::replace(state, fresh_state()),
        Msg::StreamDone {
            turn: TurnId(9),
            usage: None,
            provider_continuation: None,
            stop_reason: None,
        },
    );
    *state = next;
    cmds.iter()
        .find_map(|c| match c {
            Cmd::ExecuteTool { call_id, .. } => Some(*call_id),
            _ => None,
        })
        .expect("tool dispatched")
}

#[test]
fn exit_plan_mode_approval_transitions_out_and_seeds_the_checklist() {
    let mut state = state_with_two_exchanges();
    enter_planning(&mut state, "/tmp/project/.mermaid/plans/x.md");
    let call_id = drive_single_tool_call(&mut state, "exit_plan_mode");

    let body = "## Summary\nS\n\n## Tasks\n1. Add the flag\n2. Wire the broker\n";
    let outcome = ToolOutcome::success("The user approved the plan.", "plan approved", 0.1)
        .with_metadata(crate::ToolRunMetadata {
            detail: crate::ToolMetadata::Plan {
                path: ".mermaid/plans/x.md".to_string(),
                body: body.to_string(),
                start: true,
                fresh: false,
                fork: false,
                model: None,
            },
            ..Default::default()
        });
    let (state, cmds) = update(
        state,
        Msg::ToolFinished {
            turn: TurnId(9),
            call_id,
            outcome,
        },
    );

    assert!(state.session.plan.is_none(), "approval leaves plan mode");
    let subjects: Vec<String> = state
        .session
        .conversation
        .tasks
        .visible()
        .map(|t| t.subject.clone())
        .collect();
    assert_eq!(subjects, ["Add the flag", "Wire the broker"]);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::SyncTaskStore(store) if store.visible().count() == 2)),
        "the effect-side broker must be synced with the seeded store"
    );
    // start=true: the kickoff was committed as a user message at the tool
    // boundary and the follow-up model call fired.
    assert!(
        state
            .session
            .messages()
            .iter()
            .any(|m| m.role == MessageRole::User && m.content == "Implement the plan."),
        "auto-submit rides the queued-message drain"
    );
    assert!(cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })));
}

#[test]
fn replan_reconcile_preserves_completed_tasks() {
    use crate::checklist::{ChecklistEdit, ChecklistOrigin, ChecklistSpec, ChecklistStatus, Stamp};
    let mut state = fresh_state();
    enter_planning(&mut state, "/tmp/project/.mermaid/plans/x.md");
    // Prior round: one completed, one still open.
    let mut store = crate::checklist::ChecklistStore::default();
    let ids = store.create(
        vec![
            ChecklistSpec {
                subject: "Add the flag".into(),
                active_form: "Add the flag".into(),
                description: None,
                in_progress: false,
            },
            ChecklistSpec {
                subject: "Old open step".into(),
                active_form: "Old open step".into(),
                description: None,
                in_progress: false,
            },
        ],
        ChecklistOrigin::Model,
        Stamp::default(),
    );
    store.apply(
        &[ChecklistEdit {
            id: ids[0],
            status: Some(ChecklistStatus::Completed),
            subject: None,
            active_form: None,
            description: None,
        }],
        Stamp::default(),
    );
    state.session.conversation.tasks = store;

    let mut cmds = Vec::new();
    let body = "## Tasks\n1. Add the flag\n2. New step\n";
    finish_plan_mode(&mut state, &mut cmds, body, false);

    let visible: Vec<(String, crate::checklist::ChecklistStatus)> = state
        .session
        .conversation
        .tasks
        .visible()
        .map(|t| (t.subject.clone(), t.status))
        .collect();
    // Completed survives, is not duplicated; the open step is replaced.
    assert_eq!(
        visible,
        [
            ("Add the flag".to_string(), ChecklistStatus::Completed),
            ("New step".to_string(), ChecklistStatus::Pending),
        ]
    );
    // start=false: nothing queued.
    assert!(state.ui.queued_messages.is_empty());
}

#[test]
fn enter_plan_mode_tool_success_flips_the_session_into_planning() {
    let mut state = state_with_two_exchanges();
    let call_id = drive_single_tool_call(&mut state, "enter_plan_mode");
    assert!(state.session.plan.is_none(), "not planning during the call");
    let (state, _) = update(
        state,
        Msg::ToolFinished {
            turn: TurnId(9),
            call_id,
            outcome: ToolOutcome::success("Plan mode is on", "plan mode on", 0.0),
        },
    );
    let plan = state.session.plan.expect("tool success enters plan mode");
    assert!(plan.plan_path.starts_with("/tmp/project/.mermaid/plans"));
}

#[test]
fn plan_config_picker_cycles_values_and_persists() {
    use crate::{PlanPermLevel, PlanPermissions};
    let (mut state, _) = update(
        fresh_state(),
        Msg::Slash(SlashCmd::Plan(Some("config".into()))),
    );
    assert!(matches!(state.ui.mode, UiMode::PlanConfig { cursor: 0 }));
    let key = |code| {
        Msg::Key(Key {
            code,
            modifiers: KeyMods::NONE,
        })
    };
    // Row 0 forward: default -> strict.
    let (next, cmds) = update(state, key(KeyCode::Enter));
    assert_eq!(next.settings.plan.permissions, PlanPermissions::strict());
    assert!(
        cmds.iter().any(|c| matches!(c, Cmd::PersistPlanConfig(_))),
        "every change persists the [plan] table"
    );
    state = next;
    // Down to builds, cycle allow -> auto (strict starts at deny -> allow).
    let (next, _) = update(state, key(KeyCode::Down));
    let (next, _) = update(next, key(KeyCode::Right));
    assert_eq!(next.settings.plan.permissions.builds, PlanPermLevel::Allow);
    // Custom values flip the preset display to none.
    assert!(next.settings.plan.permissions.preset_name().is_none());
    // Esc closes.
    let (next, _) = update(next, key(KeyCode::Escape));
    assert!(matches!(next.ui.mode, UiMode::EditingInput));
}

#[test]
fn plan_model_override_swaps_on_entry_and_restores_on_exit() {
    let mut state = fresh_state();
    state.settings.plan.model = Some("anthropic/frontier".to_string());
    state.settings.plan.reasoning = Some(mermaid_model::models::ReasoningLevel::High);
    let (state, _) = update(state, Msg::Slash(SlashCmd::Plan(None)));
    assert_eq!(state.session.model_id, "anthropic/frontier");
    assert_eq!(
        state.session.reasoning,
        mermaid_model::models::ReasoningLevel::High
    );
    let plan = state.session.plan.clone().expect("planning");
    assert_eq!(plan.prev_model_id.as_deref(), Some("ollama/test"));
    // Exit restores.
    let (state, _) = update(state, Msg::Slash(SlashCmd::Plan(Some("off".into()))));
    assert_eq!(state.session.model_id, "ollama/test");
    assert_eq!(
        state.session.reasoning,
        mermaid_model::models::ReasoningLevel::Medium,
        "reasoning restored to the pre-plan default"
    );
}

#[test]
fn plan_capabilities_line_tracks_the_profile() {
    use crate::PlanPermissions;
    let line = plan_capabilities_line(&PlanPermissions::default());
    assert!(line.contains("build and test"));
    assert!(line.contains("web search/fetch"));
    let line = plan_capabilities_line(&PlanPermissions::strict());
    assert!(!line.contains("build and test"));
    assert!(!line.contains("web search/fetch"));
    assert!(
        line.contains("write_file or apply_patch"),
        "the capabilities line must name the plan-authoring tools: {line}"
    );
    assert!(line.contains("ONLY writable path"));
}

fn plan_outcome(fresh: bool, fork: bool, model: Option<&str>) -> ToolOutcome {
    ToolOutcome::success("approved", "plan approved", 0.1).with_metadata(crate::ToolRunMetadata {
        detail: crate::ToolMetadata::Plan {
            path: ".mermaid/plans/x.md".to_string(),
            body: "## Tasks\n1. Step one\n2. Step two\n".to_string(),
            start: true,
            fresh,
            fork,
            model: model.map(str::to_string),
        },
        ..Default::default()
    })
}

#[test]
fn clear_context_approval_hands_off_to_a_fresh_conversation() {
    let mut state = state_with_two_exchanges();
    enter_planning(&mut state, "/tmp/project/.mermaid/plans/x.md");
    let original_id = state.session.conversation.id.clone();
    let call_id = drive_single_tool_call(&mut state, "exit_plan_mode");
    let (state, cmds) = update(
        state,
        Msg::ToolFinished {
            turn: TurnId(9),
            call_id,
            outcome: plan_outcome(true, false, Some("ollama/executor")),
        },
    );
    assert!(state.session.plan.is_none());
    assert_ne!(
        state.session.conversation.id, original_id,
        "execution continues in a new conversation"
    );
    assert_eq!(state.session.model_id, "ollama/executor");
    assert_eq!(state.session.conversation.tasks.visible().count(), 2);
    assert!(
        cmds.iter().any(|c| matches!(c, Cmd::CancelScope(_))),
        "the exploration turn's scope is cancelled"
    );
    // The kickoff self-submitted within this update: the fresh transcript
    // holds exactly the preamble+plan user message, and its turn is live.
    let users: Vec<&ChatMessage> = state
        .session
        .messages()
        .iter()
        .filter(|m| m.role == MessageRole::User)
        .collect();
    assert_eq!(users.len(), 1, "only the kickoff rides the fresh context");
    assert!(
        users[0]
            .content
            .starts_with(crate::prompts::PLAN_HANDOFF_PREAMBLE)
    );
    assert!(users[0].content.contains("## Tasks"));
    assert!(
        matches!(state.turn, TurnState::Generating { .. }),
        "the kickoff turn starts immediately"
    );
}

#[test]
fn fork_handoff_carries_the_transcript() {
    let mut state = state_with_two_exchanges();
    enter_planning(&mut state, "/tmp/project/.mermaid/plans/x.md");
    let original_id = state.session.conversation.id.clone();
    let original_len = state.session.messages().len();
    let call_id = drive_single_tool_call(&mut state, "exit_plan_mode");
    let mid_len = state.session.messages().len();
    assert!(mid_len >= original_len, "tool_use assistant committed");
    let (state, _cmds) = update(
        state,
        Msg::ToolFinished {
            turn: TurnId(9),
            call_id,
            outcome: plan_outcome(false, true, None),
        },
    );
    assert_ne!(state.session.conversation.id, original_id);
    assert_eq!(
        state.session.conversation.forked_from.as_deref(),
        Some(original_id.as_str())
    );
    // The transcript carried over, plus the self-submitted kickoff.
    assert_eq!(
        state.session.messages().len(),
        mid_len + 1,
        "fork carries the transcript and appends the kickoff"
    );
    assert_eq!(
        state.session.messages().last().map(|m| m.content.as_str()),
        Some("Implement the plan.")
    );
    assert!(matches!(state.turn, TurnState::Generating { .. }));
}

/// A rewind used to blank the context gauge — 250k/1M became `context:
/// n/a`, which reads as "the meter broke" rather than "there is less
/// context now". The fork's context is the most precisely known thing
/// about it (the prefix the user just chose), so it is re-estimated.
#[test]
fn rewind_reestimates_the_context_gauge_instead_of_blanking_it() {
    let mut state = state_with_two_exchanges();
    state.runtime.provider_capabilities.max_context_tokens = Some(1_000_000);
    // A provider-counted figure for the FULL transcript, as a live session
    // would have after its last call.
    state.session.context_usage = Some(crate::ContextUsageSnapshot::from_usage(
        &mermaid_model::models::TokenUsage::provider(250_000, 1_000),
        Some(1_000_000),
    ));
    let before = state
        .session
        .context_usage
        .as_ref()
        .expect("seeded")
        .used_tokens;

    let mut cmds = Vec::new();
    fork_conversation_at(&mut state, &mut cmds, 2);

    let after = state
        .session
        .context_usage
        .as_ref()
        .expect("the gauge must survive a rewind");
    assert!(
        after.max_tokens == Some(1_000_000),
        "the window carries over: {:?}",
        after.max_tokens
    );
    assert!(
        after.used_tokens < before,
        "a rewind drops messages, so context must shrink: {} -> {}",
        before,
        after.used_tokens
    );
    assert!(
        after.is_estimate(),
        "no provider counted the fork yet — it must render with the ~ marker"
    );
    // Cumulative spend is NOT context: the tokens were really spent.
    assert!(state.session.last_token_usage.is_none());
}

#[test]
fn fork_fires_the_checkpoint_lookup_with_the_original_session_id() {
    let mut state = state_with_two_exchanges();
    let original_id = state.session.conversation.id.clone();
    let mut cmds = Vec::new();
    fork_conversation_at(&mut state, &mut cmds, 2);
    let found = cmds
        .iter()
        .find_map(|c| match c {
            Cmd::Query(Query::ListForkCheckpoints {
                session_id,
                message_index,
            }) => Some((session_id.clone(), *message_index)),
            _ => None,
        })
        .expect("fork queries anchored checkpoints");
    assert_eq!(found.0, original_id, "anchors reference the ORIGINAL id");
    assert_eq!(found.1, 2);
    assert_ne!(state.session.conversation.id, original_id, "forked");
}

fn anchored_checkpoint(id: &str, index: i64) -> mermaid_model::records::CheckpointRecord {
    mermaid_model::records::CheckpointRecord {
        id: id.to_string(),
        task_id: None,
        project_path: "/tmp/p".to_string(),
        snapshot_path: format!("/snap/{id}"),
        changed_files_json: "[]".to_string(),
        pending_action_json: None,
        approval_id: None,
        created_at: "2026-01-01T00:00:00Z".to_string(),
        archived_at: None,
        archive_reason: None,
        session_id: Some("sess".to_string()),
        message_index: Some(index),
    }
}

#[test]
fn fork_checkpoints_found_names_the_oldest_or_stays_silent() {
    let state = fresh_state();
    let before = state.session.messages().len();
    let (state, _) = update(
        state,
        Msg::QueryResult(QueryResult::ForkCheckpointsFound(vec![
            anchored_checkpoint("cp-old", 4),
            anchored_checkpoint("cp-new", 8),
        ])),
    );
    let notice = state.session.messages().last().expect("notice appended");
    assert!(notice.content.contains("cp-old"), "{}", notice.content);
    assert!(notice.content.contains("Files were not rewound"));
    assert!(notice.content.contains("2 file checkpoint(s)"));

    let (state, _) = update(
        state,
        Msg::QueryResult(QueryResult::ForkCheckpointsFound(Vec::new())),
    );
    assert_eq!(
        state.session.messages().len(),
        before + 1,
        "empty reply emits nothing"
    );
}

#[test]
fn mcp_server_errored_sets_status_and_emits_status_line() {
    let mut state = fresh_state();
    state.mcp.servers.insert(
        "s1".to_string(),
        McpServerEntry {
            config: crate::McpServerConfig {
                command: "echo".to_string(),
                args: vec![],
                env: std::collections::HashMap::new(),
                ..Default::default()
            },
            status: McpServerStatus::Starting,
            tools: vec![],
        },
    );
    let (state, _) = update(
        state,
        Msg::McpServerErrored {
            name: "s1".to_string(),
            reason: "exit 1".to_string(),
        },
    );
    match &state.mcp.servers["s1"].status {
        McpServerStatus::Errored { reason } => assert_eq!(reason, "exit 1"),
        _ => panic!("expected Errored"),
    }
    assert!(
        state
            .session
            .messages()
            .last()
            .is_some_and(|m| m.content.contains("MCP server s1 errored: exit 1")),
        "the MCP error must be posted to the chat transcript"
    );
}

#[test]
fn push_system_during_compacting_inserts_before_tool_call_pair() {
    // D1: while Compacting with a trailing committed `assistant(tool_calls)`
    // (a context-limit compaction preserves the unpaired tool_use), an
    // `McpServerErrored` note must be inserted BEFORE that assistant message,
    // not appended after it — keeping the tool_use adjacent to its tool_result.
    let mut state = fresh_state();
    let source = mermaid_model::models::tool_call::ToolCall {
        id: Some("call-1".to_string()),
        function: mermaid_model::models::tool_call::FunctionCall {
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "foo"}),
        },
    };
    state.session.append(
        ChatMessage::assistant("running a tool").with_tool_calls(vec![source]),
        state.now,
    );
    state.turn = TurnState::Compacting {
        id: TurnId(9),
        started: std::time::SystemTime::now(),
        trigger: CompactionTrigger::ContextLimitRetry,
        resume_continuation: false,
    };

    let (state, _) = update(
        state,
        Msg::McpServerErrored {
            name: "s1".to_string(),
            reason: "exit 1".to_string(),
        },
    );

    let messages = state.session.messages();
    let n = messages.len();
    assert!(
        n >= 2
            && messages[n - 1].role == MessageRole::Assistant
            && messages[n - 1].tool_calls.is_some(),
        "the assistant(tool_calls) must stay last so its tool_result can follow"
    );
    assert!(
        messages[n - 2].role == MessageRole::System
            && messages[n - 2].content.contains("MCP server s1 errored"),
        "the system note sits directly before the tool-call pair, not after it"
    );
}

#[test]
fn tool_finished_with_all_outcomes_triggers_follow_up_call_model() {
    let mut state = fresh_state();
    let call = PendingToolCall {
        call_id: mermaid_model::ids::ToolCallId(1),
        source: mermaid_model::models::tool_call::ToolCall {
            id: Some("c1".to_string()),
            function: mermaid_model::models::tool_call::FunctionCall {
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "foo"}),
            },
        },
    };
    state.turn = start_executing_tools(TurnId(3), vec![call], std::time::SystemTime::now());
    // The reducer looks up the "last assistant message" to attach
    // an ActionDisplay — plant one so the lookup doesn't silently
    // no-op in this test.
    state
        .session
        .append(ChatMessage::assistant("tools follow"), state.now);

    let (state, cmds) = update(
        state,
        Msg::ToolFinished {
            turn: TurnId(3),
            call_id: mermaid_model::ids::ToolCallId(1),
            outcome: ToolOutcome::success("file contents", "file contents", 0.05),
        },
    );

    assert!(matches!(state.turn, TurnState::Generating { .. }));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })));
    // Tool result message was appended.
    let last = state.session.messages().last().unwrap();
    assert_eq!(last.role, MessageRole::Tool);
}

/// A finished `execute_command` must request a full repaint — a shell
/// child may have scribbled on the terminal behind ratatui's back buffer.
#[test]
fn exec_tool_finished_bumps_full_redraw_seq() {
    let mut state = fresh_state();
    let call = PendingToolCall {
        call_id: mermaid_model::ids::ToolCallId(1),
        source: mermaid_model::models::tool_call::ToolCall {
            id: Some("c1".to_string()),
            function: mermaid_model::models::tool_call::FunctionCall {
                name: "execute_command".to_string(),
                arguments: serde_json::json!({"command": "echo hi"}),
            },
        },
    };
    state.turn = start_executing_tools(TurnId(3), vec![call], std::time::SystemTime::now());
    let before = state.ui.full_redraw_seq;

    let (state, _cmds) = update(
        state,
        Msg::ToolFinished {
            turn: TurnId(3),
            call_id: mermaid_model::ids::ToolCallId(1),
            outcome: ToolOutcome::success("hi", "hi", 0.01),
        },
    );

    assert_eq!(
        state.ui.full_redraw_seq,
        before.wrapping_add(1),
        "execute_command completion must bump the repaint counter"
    );
}

/// Tools that can't touch the tty must NOT trigger a repaint — clearing
/// on every tool completion would flash during rapid read/edit loops.
#[test]
fn non_exec_tool_finished_does_not_bump_full_redraw_seq() {
    let mut state = fresh_state();
    let call = PendingToolCall {
        call_id: mermaid_model::ids::ToolCallId(1),
        source: mermaid_model::models::tool_call::ToolCall {
            id: Some("c1".to_string()),
            function: mermaid_model::models::tool_call::FunctionCall {
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "foo"}),
            },
        },
    };
    state.turn = start_executing_tools(TurnId(3), vec![call], std::time::SystemTime::now());
    let before = state.ui.full_redraw_seq;

    let (state, _cmds) = update(
        state,
        Msg::ToolFinished {
            turn: TurnId(3),
            call_id: mermaid_model::ids::ToolCallId(1),
            outcome: ToolOutcome::success("contents", "contents", 0.01),
        },
    );

    assert_eq!(state.ui.full_redraw_seq, before);
}

#[test]
fn ctrl_l_bumps_full_redraw_seq() {
    let state = fresh_state();
    let before = state.ui.full_redraw_seq;
    let (state, cmds) = update(
        state,
        Msg::Key(Key {
            code: KeyCode::Char('l'),
            modifiers: KeyMods {
                ctrl: true,
                ..KeyMods::NONE
            },
        }),
    );
    assert_eq!(state.ui.full_redraw_seq, before.wrapping_add(1));
    assert!(!state.should_exit, "Ctrl+L must not exit");
    assert!(cmds.is_empty(), "Ctrl+L is reducer-only: {cmds:?}");
}

#[test]
fn ctrl_t_toggles_the_task_checklist() {
    let ctrl_t = || {
        Msg::Key(Key {
            code: KeyCode::Char('t'),
            modifiers: KeyMods {
                ctrl: true,
                ..KeyMods::NONE
            },
        })
    };
    let state = fresh_state();
    assert!(!state.ui.tasks_collapsed, "expanded by default");
    let (state, cmds) = update(state, ctrl_t());
    assert!(state.ui.tasks_collapsed);
    assert!(cmds.is_empty(), "Ctrl+T is reducer-only: {cmds:?}");
    let (state, _) = update(state, ctrl_t());
    assert!(!state.ui.tasks_collapsed, "second press expands again");
}

fn sample_task_store(statuses: &[crate::ChecklistStatus]) -> crate::ChecklistStore {
    use crate::checklist::{ChecklistEdit, ChecklistSpec, Stamp};
    let mut store = crate::ChecklistStore::default();
    store.create(
        statuses
            .iter()
            .enumerate()
            .map(|(i, _)| ChecklistSpec {
                subject: format!("task {i}"),
                active_form: format!("doing {i}"),
                description: None,
                in_progress: false,
            })
            .collect(),
        crate::ChecklistOrigin::Model,
        Stamp::default(),
    );
    let edits: Vec<ChecklistEdit> = statuses
        .iter()
        .enumerate()
        .filter(|(_, s)| **s != crate::ChecklistStatus::Pending)
        .map(|(i, s)| ChecklistEdit {
            id: (i + 1) as u32,
            status: Some(*s),
            ..ChecklistEdit::default()
        })
        .collect();
    store.apply(&edits, Stamp::default());
    store
}

#[test]
fn tasks_updated_replaces_snapshot_and_diffs_completions() {
    use crate::ChecklistStatus::{Completed, InProgress, Pending};
    let state = fresh_state();
    // First snapshot: nothing completed — no notifications.
    let (state, cmds) = update(
        state,
        Msg::TasksUpdated {
            store: sample_task_store(&[InProgress, Pending]),
        },
    );
    assert!(cmds.is_empty(), "{cmds:?}");
    assert_eq!(state.session.conversation.tasks.visible().count(), 2);

    // Task 1 completes: exactly one notification, with counts.
    let (state, cmds) = update(
        state,
        Msg::TasksUpdated {
            store: sample_task_store(&[Completed, InProgress]),
        },
    );
    let notes: Vec<_> = cmds
        .iter()
        .filter(|c| matches!(c, Cmd::NotifyTaskCompleted { .. }))
        .collect();
    assert_eq!(notes.len(), 1);
    if let Cmd::NotifyTaskCompleted {
        task,
        completed,
        total,
    } = notes[0]
    {
        assert_eq!(task.id, 1);
        assert_eq!((*completed, *total), (1, 2));
    }

    // The same snapshot re-sent: the diff is the dedupe — no re-fire.
    let (_state, cmds) = update(
        state,
        Msg::TasksUpdated {
            store: sample_task_store(&[Completed, InProgress]),
        },
    );
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::NotifyTaskCompleted { .. })),
        "identical snapshot must not re-notify: {cmds:?}"
    );
}

#[test]
fn fork_clears_tasks_and_syncs_the_broker() {
    use crate::ChecklistStatus::InProgress;
    let state = fresh_state();
    let (mut state, _) = update(
        state,
        Msg::TasksUpdated {
            store: sample_task_store(&[InProgress]),
        },
    );
    // Two committed user messages so index 1 is a valid fork cut.
    state
        .session
        .append(mermaid_model::models::ChatMessage::user("one"), state.now);
    state
        .session
        .append(mermaid_model::models::ChatMessage::user("two"), state.now);
    let mut cmds = Vec::new();
    super::fork_conversation_at(&mut state, &mut cmds, 1);
    assert!(state.session.conversation.tasks.is_empty());
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Cmd::SyncTaskStore(store) if store.tasks.is_empty()
        )),
        "fork must clear the broker too: {cmds:?}"
    );
}

#[test]
fn todos_command_routes_edits_and_prints_list() {
    // Bare /todos on an empty list: helpful system line, no Cmd.
    let state = fresh_state();
    let (state, cmds) = update(state, Msg::Slash(SlashCmd::Todos(None)));
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::UserTaskEdit(_))));
    assert!(
        state
            .session
            .messages()
            .last()
            .is_some_and(|m| m.content.contains("No tasks")),
    );

    // add routes through the broker command, never mutating state directly.
    let (state, cmds) = update(
        state,
        Msg::Slash(SlashCmd::Todos(Some("add review the docs".to_string()))),
    );
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::UserTaskEdit(crate::UserChecklistEdit::Add { subject }) if subject == "review the docs"
    )));
    assert!(state.session.conversation.tasks.is_empty());

    // done accepts a #-prefixed id; garbage prints usage.
    let (state, cmds) = update(
        state,
        Msg::Slash(SlashCmd::Todos(Some("done #3".to_string()))),
    );
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::UserTaskEdit(crate::UserChecklistEdit::Done { id: 3 })
    )));
    let (state, _) = update(
        state,
        Msg::Slash(SlashCmd::Todos(Some("frobnicate".to_string()))),
    );
    assert!(
        state
            .session
            .messages()
            .last()
            .is_some_and(|m| m.content.contains("usage: /todos")),
    );
}

#[test]
fn task_notices_ride_the_next_request_then_clear() {
    let state = fresh_state();
    let (mut state, _) = update(
        state,
        Msg::TaskNotice {
            text: "The user edited the task checklist: Added task #1 'x'.".to_string(),
        },
    );
    let mut cmds = Vec::new();
    super::push_call_model(&mut state, &mut cmds, TurnId(1));
    let Some(Cmd::CallModel { request, .. }) = cmds.first() else {
        panic!("expected CallModel: {cmds:?}");
    };
    let instructions = request.instructions.clone().unwrap_or_default();
    assert!(instructions.contains("# Task Checklist Notices"));
    assert!(instructions.contains("Added task #1 'x'"));
    assert!(
        state.pending_task_notices.is_empty(),
        "notices are consumed by the dispatch"
    );
}

#[test]
fn stale_in_progress_task_triggers_a_nudge_every_n_calls() {
    use crate::ChecklistStatus::InProgress;
    let state = fresh_state();
    let (mut state, _) = update(
        state,
        Msg::TasksUpdated {
            store: sample_task_store(&[InProgress]),
        },
    );
    // Calls 1..4: no nudge. Call 5: nudge rides the request and re-arms.
    for i in 1..=4 {
        let mut cmds = Vec::new();
        super::push_call_model(&mut state, &mut cmds, TurnId(i));
        let Some(Cmd::CallModel { request, .. }) = cmds.first() else {
            panic!("expected CallModel");
        };
        assert!(
            !request
                .instructions
                .clone()
                .unwrap_or_default()
                .contains("in_progress for"),
            "no nudge before the threshold (call {i})"
        );
    }
    let mut cmds = Vec::new();
    super::push_call_model(&mut state, &mut cmds, TurnId(5));
    let Some(Cmd::CallModel { request, .. }) = cmds.first() else {
        panic!("expected CallModel");
    };
    let instructions = request.instructions.clone().unwrap_or_default();
    assert!(
        instructions.contains("Task #1 'task 0' has been in_progress"),
        "nudge fires at the threshold: {instructions}"
    );
    assert_eq!(state.runtime.calls_since_task_update, 0, "re-armed");

    // A checklist update resets the counter.
    let (state, _) = update(
        state,
        Msg::TasksUpdated {
            store: sample_task_store(&[InProgress]),
        },
    );
    assert_eq!(state.runtime.calls_since_task_update, 0);
}

/// Ctrl+L is meta-level (like Ctrl+C/Ctrl+B): it must work — and only
/// repaint — while an approval modal is open.
#[test]
fn ctrl_l_works_during_approval_modal() {
    let state = pending_approval_state();
    let before = state.ui.full_redraw_seq;
    let (state, cmds) = update(
        state,
        Msg::Key(Key {
            code: KeyCode::Char('l'),
            modifiers: KeyMods {
                ctrl: true,
                ..KeyMods::NONE
            },
        }),
    );
    assert_eq!(state.ui.full_redraw_seq, before.wrapping_add(1));
    assert_eq!(
        state.pending_approval.len(),
        1,
        "the approval must remain queued, not be resolved by Ctrl+L"
    );
    assert!(cmds.is_empty());
}

fn test_attachment(id: u64) -> crate::Attachment {
    crate::Attachment {
        id,
        // Mirror id → number so a test can reference the pill as `[Image #id]`.
        number: id,
        base64_data: "AAAA".to_string(),
        temp_path: PathBuf::from(format!("/tmp/a{id}.png")),
        size_bytes: 4,
        format: "png".to_string(),
    }
}

fn generating(id: u64, partial: &str) -> TurnState {
    TurnState::Generating {
        id: TurnId(id),
        started: std::time::SystemTime::now(),
        partial_text: partial.to_string(),
        partial_reasoning: String::new(),
        tokens: 0,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: Vec::new(),
        continuation: false,
    }
}

#[test]
fn queued_message_keeps_attachments_from_queue_time() {
    // Axis 1 #1: a message queued while busy must re-submit with the
    // attachments present when it was queued, not whatever is live when the
    // FIFO drains.
    let mut state = fresh_state();
    state.turn = generating(5, "answer");
    state.ui.attachments.push(test_attachment(1)); // id 1, number 1

    // Busy → queued, capturing id 1. The text carries its inline pill.
    let (mut state, _) = update(
        state,
        Msg::SubmitPrompt {
            text: "[Image #1] queued".to_string(),
            attachment_ids: vec![1],
        },
    );
    assert_eq!(state.ui.queued_messages.len(), 1);

    // User preps a different image for the NEXT message while the turn runs.
    state.ui.attachments.push(test_attachment(2)); // id 2, number 2

    // Turn completes → queued message drains and re-submits.
    let (state, _) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(5),
            usage: None,
            provider_continuation: None,
            stop_reason: None,
        },
    );

    // The queued message consumed image #1 (matched by its token + queued id
    // scope); the live id 2 is untouched — proving the queue-time set was
    // used, not whatever is live at drain.
    assert_eq!(state.ui.attachments.len(), 1);
    assert_eq!(state.ui.attachments[0].id, 2);
    let queued_msg = state
        .session
        .messages()
        .iter()
        .find(|m| m.role == MessageRole::User && m.content == "[Image #1] queued")
        .expect("queued message submitted");
    assert_eq!(queued_msg.image_numbers, Some(vec![1]));
}

#[test]
fn stream_done_without_usage_keeps_previous_last_token_usage() {
    // Axis 1 #2: a turn reporting no usage must not wipe the last request's
    // usage to "n/a".
    let mut state = fresh_state();
    state.turn = generating(1, "first");
    let (mut state, _) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(1),
            usage: Some(mermaid_model::models::TokenUsage::provider(120, 30)),
            provider_continuation: None,
            stop_reason: None,
        },
    );
    assert_eq!(state.session.last_token_usage.unwrap().prompt_tokens, 120);

    // A second turn reports no usage (common on tool follow-ups).
    state.turn = generating(2, "second");
    let (state, _) = update(
        state,
        Msg::StreamDone {
            turn: TurnId(2),
            usage: None,
            provider_continuation: None,
            stop_reason: None,
        },
    );
    assert_eq!(
        state
            .session
            .last_token_usage
            .expect("retained")
            .prompt_tokens,
        120
    );
}

#[test]
fn stream_tool_call_outside_generating_is_dropped_without_panic() {
    // Axis 1 #5: a tool-call event arriving after the turn left Generating
    // is dropped (and logged), never panics or mutates state.
    let mut state = fresh_state();
    let call = PendingToolCall {
        call_id: mermaid_model::ids::ToolCallId(1),
        source: mermaid_model::models::tool_call::ToolCall {
            id: Some("c1".to_string()),
            function: mermaid_model::models::tool_call::FunctionCall {
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "foo"}),
            },
        },
    };
    state.turn = start_executing_tools(TurnId(3), vec![call], std::time::SystemTime::now());
    let (state, cmds) = update(
        state,
        Msg::StreamToolCall {
            turn: TurnId(3),
            call: mermaid_model::models::tool_call::ToolCall {
                id: Some("late".to_string()),
                function: mermaid_model::models::tool_call::FunctionCall {
                    name: "write_file".to_string(),
                    arguments: serde_json::json!({}),
                },
            },
        },
    );
    assert!(matches!(state.turn, TurnState::ExecutingTools { .. }));
    assert!(cmds.is_empty());
}

#[test]
fn exit_commits_interrupted_partial_before_saving() {
    // Axis 1 #6: quitting mid-stream preserves the partial assistant reply
    // (with an interrupted marker) so `--continue` shows what was on screen.
    let mut state = fresh_state();
    state.turn = generating(1, "half written");
    let (state, cmds) = update(state, Msg::Quit);
    assert!(state.should_exit);
    let last = state.session.messages().last().expect("a message");
    assert_eq!(last.role, MessageRole::Assistant);
    assert!(last.content.contains("half written"));
    assert!(last.content.contains("[interrupted]"));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::SaveConversation { .. }))
    );
}

#[test]
fn backgrounded_tool_completes_turn_not_stranded() {
    // Axis 1 #8 (verified non-bug): Ctrl+B fires BackgroundScope but leaves
    // the reducer in ExecutingTools; the detachable tool still returns a
    // success outcome, so the turn advances normally. Locks that behavior.
    let mut state = fresh_state();
    let call = PendingToolCall {
        call_id: mermaid_model::ids::ToolCallId(1),
        source: mermaid_model::models::tool_call::ToolCall {
            id: Some("c1".to_string()),
            function: mermaid_model::models::tool_call::FunctionCall {
                name: "execute_command".to_string(),
                arguments: serde_json::json!({"command": "sleep 9"}),
            },
        },
    };
    state.turn = start_executing_tools(TurnId(3), vec![call], std::time::SystemTime::now());
    state
        .session
        .append(ChatMessage::assistant("tools follow"), state.now);

    // Ctrl+B → BackgroundScope; reducer stays in ExecutingTools.
    let (state, cmds) = update(
        state,
        Msg::Key(Key {
            code: KeyCode::Char('b'),
            modifiers: KeyMods::ctrl(),
        }),
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::BackgroundScope(TurnId(3))))
    );
    assert!(matches!(state.turn, TurnState::ExecutingTools { .. }));

    // The detached command returns a normal success outcome → turn advances.
    let (state, cmds) = update(
        state,
        Msg::ToolFinished {
            turn: TurnId(3),
            call_id: mermaid_model::ids::ToolCallId(1),
            outcome: ToolOutcome::success(
                "Moved to background.\nPID: 1234",
                "moved to background",
                0.1,
            ),
        },
    );
    assert!(matches!(state.turn, TurnState::Generating { .. }));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })));
}

#[test]
fn builtin_tool_schema_tokens_msg_updates_runtime() {
    // Axis 1 #4: the runner's report lands on runtime state.
    let state = fresh_state();
    let (state, _) = update(state, Msg::BuiltinToolSchemaTokens(4321));
    assert_eq!(state.runtime.builtin_tool_schema_tokens, 4321);
}

#[test]
fn context_text_folds_in_builtin_tool_tokens() {
    // Axis 1 #4: /context shows a disclaimer before the runner reports, and
    // the real figure afterward.
    let mut state = fresh_state();
    let before = context_text(&state);
    assert!(before.contains("built-in tool schemas: measured on the first model call"));

    state.runtime.builtin_tool_schema_tokens = 5000;
    let after = context_text(&state);
    assert!(after.contains("built-in tool schemas:"));
    assert!(!after.contains("measured on the first model call"));
}

#[test]
fn context_text_shows_ollama_window_detail_and_tip() {
    use mermaid_model::models::adapters::ollama_sizing::NumCtxSource;
    use mermaid_model::tool_run::OllamaContextInfo;
    let mut state = fresh_state();
    // No probe yet → no Ollama window lines.
    assert!(!context_text(&state).contains("Active num_ctx"));

    state.runtime.ollama_context = Some(OllamaContextInfo {
        model_max: Some(262_144),
        effective: Some(12_288),
        source: Some(NumCtxSource::Auto),
    });
    let text = context_text(&state);
    assert!(text.contains("Model max window"));
    assert!(text.contains("Active num_ctx"));
    assert!(text.contains("(auto"));
    assert!(text.contains("Output budget (num_predict)"));
    assert!(text.contains("RAM offload: off"));
    // Auto-fit capped well below the model's max → point to the override.
    assert!(text.contains("/context max"));

    // Once auto-converge has picked a fitting window, label it as Mermaid's
    // choice ("GPU-fit"), not the user's "(override)".
    state
        .runtime
        .ollama_converged_num_ctx
        .insert("ollama/test".to_string(), 8_192);
    let text = context_text(&state);
    assert!(text.contains("auto (GPU-fit)"), "got: {text}");
    assert!(!text.contains("(override)"));
}

#[test]
fn background_command_tool_finish_registers_process() {
    let mut state = fresh_state();
    let call = PendingToolCall {
        call_id: mermaid_model::ids::ToolCallId(1),
        source: mermaid_model::models::tool_call::ToolCall {
            id: Some("c1".to_string()),
            function: mermaid_model::models::tool_call::FunctionCall {
                name: "execute_command".to_string(),
                arguments: serde_json::json!({
                    "command": "npm run dev",
                    "mode": "background",
                    "working_dir": "/tmp/project",
                }),
            },
        },
    };
    state.turn = start_executing_tools(TurnId(3), vec![call], std::time::SystemTime::now());
    state
        .session
        .append(ChatMessage::assistant("tools follow"), state.now);

    let (state, _) = update(
            state,
            Msg::ToolFinished {
                turn: TurnId(3),
                call_id: mermaid_model::ids::ToolCallId(1),
                outcome: ToolOutcome::success(
                    "Background command started.\nPID: 123\nLog: /tmp/mermaid-bg.log\nReady: matched pattern \"Local:\"\nDetected URL: http://127.0.0.1:5173\n",
                    "background process started",
                    0.2,
                )
                .with_metadata(crate::ToolRunMetadata {
                    process: Some(crate::ManagedProcess {
                        id: "bg-123".to_string(),
                        pid: 123,
                        command: "npm run dev".to_string(),
                        cwd: Some("/tmp/project".to_string()),
                        log_path: "/tmp/mermaid-bg.log".to_string(),
                        detected_url: Some("http://127.0.0.1:5173".to_string()),
                        status: mermaid_model::records::ProcessStatus::Running,
                    }),
                    ..crate::ToolRunMetadata::default()
                }),
            },
        );

    assert_eq!(state.runtime.processes.len(), 1);
    let process = &state.runtime.processes[0];
    assert_eq!(process.pid, 123);
    assert_eq!(process.command, "npm run dev");
    assert_eq!(process.cwd.as_deref(), Some("/tmp/project"));
    assert_eq!(
        process.detected_url.as_deref(),
        Some("http://127.0.0.1:5173")
    );
}

#[test]
fn tool_finished_partial_stays_in_executing() {
    let mut state = fresh_state();
    let calls = vec![
        PendingToolCall {
            call_id: mermaid_model::ids::ToolCallId(1),
            source: mermaid_model::models::tool_call::ToolCall {
                id: Some("c1".to_string()),
                function: mermaid_model::models::tool_call::FunctionCall {
                    name: "read_file".to_string(),
                    arguments: serde_json::json!({}),
                },
            },
        },
        PendingToolCall {
            call_id: mermaid_model::ids::ToolCallId(2),
            source: mermaid_model::models::tool_call::ToolCall {
                id: Some("c2".to_string()),
                function: mermaid_model::models::tool_call::FunctionCall {
                    name: "write_file".to_string(),
                    arguments: serde_json::json!({}),
                },
            },
        },
    ];
    state.turn = start_executing_tools(TurnId(3), calls, std::time::SystemTime::now());
    state
        .session
        .append(ChatMessage::assistant("tools follow"), state.now);

    let (state, cmds) = update(
        state,
        Msg::ToolFinished {
            turn: TurnId(3),
            call_id: mermaid_model::ids::ToolCallId(1),
            outcome: ToolOutcome::cancelled(),
        },
    );

    // Still in ExecutingTools (second tool pending).
    match &state.turn {
        TurnState::ExecutingTools { outcomes, .. } => {
            assert_eq!(outcomes.len(), 2);
            assert!(outcomes[0].is_some());
            assert!(outcomes[1].is_none());
        },
        _ => panic!("should still be ExecutingTools"),
    }
    assert!(cmds.is_empty());
}

#[test]
fn stale_tool_finished_dropped_silently() {
    let mut state = fresh_state();
    state.turn = start_executing_tools(
        TurnId(3),
        vec![PendingToolCall {
            call_id: mermaid_model::ids::ToolCallId(1),
            source: mermaid_model::models::tool_call::ToolCall {
                id: None,
                function: mermaid_model::models::tool_call::FunctionCall {
                    name: "x".to_string(),
                    arguments: serde_json::json!({}),
                },
            },
        }],
        std::time::SystemTime::now(),
    );

    let (state, cmds) = update(
        state,
        Msg::ToolFinished {
            turn: TurnId(999),
            call_id: mermaid_model::ids::ToolCallId(1),
            outcome: ToolOutcome::cancelled(),
        },
    );
    match &state.turn {
        TurnState::ExecutingTools { outcomes, .. } => {
            assert!(outcomes[0].is_none());
        },
        _ => panic!("unchanged state expected"),
    }
    assert!(cmds.is_empty());
}

#[test]
fn background_agent_lifecycle_registry_note_queue_and_usage() {
    // Started: adds a live-panel registry row.
    let state = fresh_state();
    let (state, _) = update(
        state,
        Msg::BackgroundAgentStarted {
            agent_id: "a3".to_string(),
            description: "audit docs".to_string(),
        },
    );
    assert_eq!(state.runtime.background_agents.len(), 1);
    assert_eq!(state.runtime.background_agents[0].description, "audit docs");

    // Progress: updates activity/tokens on the row.
    let (state, _) = update(
        state,
        Msg::BackgroundAgentProgress {
            agent_id: "a3".to_string(),
            activity: "read_file…".to_string(),
            tokens: 4_200,
        },
    );
    assert_eq!(state.runtime.background_agents[0].activity, "read_file…");
    assert_eq!(state.runtime.background_agents[0].tokens, 4_200);

    // Finished while IDLE: row removed, usage folded into session totals,
    // and the report auto-submits through the queued-message path (the
    // outer update() drains pending_msgs, so the turn starts immediately).
    let tokens_before = state.session.cumulative_token_usage.total_tokens();
    let (state, _) = update(
        state,
        Msg::BackgroundAgentFinished {
            agent_id: "a3".to_string(),
            description: "audit docs".to_string(),
            report: "docs are fine".to_string(),
            success: true,
            cancelled: false,
            usage: Some(mermaid_model::models::TokenUsage::provider(70_000, 20_000)),
            tokens: 90_000,
            duration_secs: 61,
        },
    );
    assert!(state.runtime.background_agents.is_empty());
    assert_eq!(
        state.session.cumulative_token_usage.total_tokens(),
        tokens_before + 90_000
    );
    assert!(
        !matches!(state.turn, TurnState::Idle),
        "idle delivery must auto-submit the report"
    );
    let last_user = state
        .session
        .messages()
        .iter()
        .rev()
        .find(|m| m.role == mermaid_model::models::MessageRole::User)
        .expect("report submitted as a user message");
    assert!(last_user.content.contains("docs are fine"));
    assert!(last_user.content.contains("background agent 'audit docs'"));
}

#[test]
fn background_agent_report_waits_in_queue_while_a_turn_runs() {
    let (state, _call_id) = state_executing_agent_call();
    let (state, _) = update(
        state,
        Msg::BackgroundAgentFinished {
            agent_id: "a9".to_string(),
            description: "security sweep".to_string(),
            report: "no findings".to_string(),
            success: true,
            cancelled: false,
            usage: None,
            tokens: 1_000,
            duration_secs: 5,
        },
    );
    // Busy turn: the report queues instead of interrupting.
    assert_eq!(state.ui.queued_messages.len(), 1);
    assert!(
        state.ui.queued_messages[0].text.contains("no findings"),
        "queued report carries the child's output"
    );
    assert!(matches!(state.turn, TurnState::ExecutingTools { .. }));
}

/// Seed a state with one detached background agent in the registry.
fn state_with_background_agent(agent_id: &str, description: &str) -> State {
    let (state, _) = update(
        fresh_state(),
        Msg::BackgroundAgentStarted {
            agent_id: agent_id.to_string(),
            description: description.to_string(),
        },
    );
    state
}

#[test]
fn slash_agents_lists_registry_or_reports_none() {
    // Empty registry: a "none" note, no Cmd beyond the transcript save.
    let (state, _) = update(fresh_state(), Msg::Slash(SlashCmd::Agents(None)));
    let last = state.session.messages().last().expect("system note");
    assert!(last.content.contains("No background agents"));

    // Populated registry: one line per agent with id + description.
    let state = state_with_background_agent("a3", "audit docs");
    let (state, _) = update(state, Msg::Slash(SlashCmd::Agents(None)));
    let last = state.session.messages().last().expect("listing");
    assert!(last.content.contains("Background agents (1)"));
    assert!(last.content.contains("a3"));
    assert!(last.content.contains("audit docs"));
    // Listing must not kill anything.
    assert_eq!(state.runtime.background_agents.len(), 1);
}

#[test]
fn slash_agents_kill_validates_id_and_fires_cmd() {
    // Unknown id: feedback, no kill Cmd.
    let state = state_with_background_agent("a3", "audit docs");
    let (state, cmds) = update(
        state,
        Msg::Slash(SlashCmd::Agents(Some("kill a99".to_string()))),
    );
    let last = state.session.messages().last().expect("note");
    assert!(last.content.contains("No background agent 'a99'"));
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::KillBackgroundAgent { .. })),
        "unknown id must not fire a kill"
    );

    // Known id: row marked cancelling, targeted kill Cmd emitted.
    let (state, cmds) = update(
        state,
        Msg::Slash(SlashCmd::Agents(Some("kill a3".to_string()))),
    );
    assert_eq!(state.runtime.background_agents[0].activity, "cancelling…");
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::KillBackgroundAgent { agent_id: Some(id) } if id == "a3"
    )));

    // Kill all: every row marked, broadcast Cmd emitted.
    let (state, cmds) = update(
        state,
        Msg::Slash(SlashCmd::Agents(Some("kill all".to_string()))),
    );
    assert!(
        state
            .runtime
            .background_agents
            .iter()
            .all(|a| a.activity == "cancelling…")
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::KillBackgroundAgent { agent_id: None }))
    );
}

#[test]
fn cancelled_background_agent_notes_but_never_queues_a_report() {
    let state = state_with_background_agent("a3", "audit docs");
    let tokens_before = state.session.cumulative_token_usage.total_tokens();
    let (state, _) = update(
        state,
        Msg::BackgroundAgentFinished {
            agent_id: "a3".to_string(),
            description: "audit docs".to_string(),
            report: "partial findings".to_string(),
            success: false,
            cancelled: true,
            usage: Some(mermaid_model::models::TokenUsage::provider(10_000, 5_000)),
            tokens: 15_000,
            duration_secs: 42,
        },
    );
    // Row cleared, spend still folded (the work was billed).
    assert!(state.runtime.background_agents.is_empty());
    assert_eq!(
        state.session.cumulative_token_usage.total_tokens(),
        tokens_before + 15_000
    );
    // Cancelled note in the transcript, but NO queued report and no
    // auto-submitted turn — a deliberate kill must not spend a model call.
    let last = state.session.messages().last().expect("note");
    assert!(last.content.contains("cancelled"));
    assert!(state.ui.queued_messages.is_empty());
    assert!(matches!(state.turn, TurnState::Idle));
}

/// Build a one-call `ExecutingTools` state around an `agent` tool call,
/// shared by the subagent progress/rollup tests below.
fn state_executing_agent_call() -> (State, mermaid_model::ids::ToolCallId) {
    let mut state = fresh_state();
    let call_id = mermaid_model::ids::ToolCallId(1);
    state.turn = start_executing_tools(
        TurnId(3),
        vec![PendingToolCall {
            call_id,
            source: mermaid_model::models::tool_call::ToolCall {
                id: None,
                function: mermaid_model::models::tool_call::FunctionCall {
                    name: "agent".to_string(),
                    arguments: serde_json::json!({"description": "explore"}),
                },
            },
        }],
        std::time::SystemTime::now(),
    );
    state
        .session
        .append(ChatMessage::assistant("spawning"), state.now);
    (state, call_id)
}

#[test]
fn subagent_progress_feeds_live_status_and_finish_clears_it() {
    let (state, call_id) = state_executing_agent_call();

    // A child tool starting shows as "<tool>…" on the parent call.
    let (state, _) = update(
        state,
        Msg::ToolProgress {
            turn: TurnId(3),
            call_id,
            event: ProgressEvent::SubagentToolCall {
                child_call_id: mermaid_model::ids::ToolCallId(9),
                tool_name: "read_file".to_string(),
                phase: SubagentPhase::Started,
            },
        },
    );
    assert_eq!(
        state
            .ui
            .live_tool_status
            .get(&call_id)
            .map(|s| s.activity.as_str()),
        Some("read_file…"),
    );

    // Coarse phase changes overwrite the activity; token counts land on
    // the same entry without touching it.
    let (state, _) = update(
        state,
        Msg::ToolProgress {
            turn: TurnId(3),
            call_id,
            event: ProgressEvent::SubagentActivity("thinking".to_string()),
        },
    );
    let (state, _) = update(
        state,
        Msg::ToolProgress {
            turn: TurnId(3),
            call_id,
            event: ProgressEvent::SubagentTokens(1234),
        },
    );
    assert_eq!(
        state.ui.live_tool_status.get(&call_id),
        Some(&crate::LiveToolStatus {
            activity: "thinking".to_string(),
            tokens: 1234,
        }),
    );

    // Progress for a stale turn must not touch the live map.
    let (state, _) = update(
        state,
        Msg::ToolProgress {
            turn: TurnId(999),
            call_id,
            event: ProgressEvent::SubagentActivity("late straggler".to_string()),
        },
    );
    let (state, _) = update(
        state,
        Msg::ToolProgress {
            turn: TurnId(999),
            call_id,
            event: ProgressEvent::SubagentTokens(9_999_999),
        },
    );
    assert_eq!(
        state
            .ui
            .live_tool_status
            .get(&call_id)
            .map(|s| (s.activity.as_str(), s.tokens)),
        Some(("thinking", 1234)),
    );

    // The call finishing removes its entry (and here ends the turn).
    let (state, _) = update(
        state,
        Msg::ToolFinished {
            turn: TurnId(3),
            call_id,
            outcome: ToolOutcome::success("report", "subagent completed", 0.5),
        },
    );
    assert!(
        state.ui.live_tool_status.is_empty(),
        "live status must not outlive the call",
    );
}

#[test]
fn subagent_usage_rolls_into_session_totals_and_run_counter() {
    let (state, call_id) = state_executing_agent_call();
    let before_cum = state.session.cumulative_token_usage.total_tokens();
    assert_eq!(state.runtime.run_tokens.output_tokens, 0);

    let usage = mermaid_model::models::TokenUsage::provider(1_000, 250).with_reasoning_output(50);
    let metadata = crate::ToolRunMetadata {
        detail: crate::ToolMetadata::Subagent {
            model_id: "ollama/test".to_string(),
            agent_id: "a1".to_string(),
        },
        token_usage: Some(usage),
        ..Default::default()
    };
    let (state, _) = update(
        state,
        Msg::ToolFinished {
            turn: TurnId(3),
            call_id,
            outcome: ToolOutcome::success("report", "subagent completed", 1.0)
                .with_metadata(metadata),
        },
    );

    // Session totals count the child's whole session (reasoning is
    // disjoint from completion, so 1000 + 250 + 50)…
    assert_eq!(
        state.session.cumulative_token_usage.total_tokens(),
        before_cum + 1_300
    );
    assert_eq!(state.session.cumulative_token_usage.completion_tokens, 250);
    // …the run counter banks its generated tokens (completion + reasoning),
    // as a real provider count (no `~` taint)…
    assert_eq!(state.runtime.run_tokens.output_tokens, 300);
    assert!(!state.runtime.run_tokens.contains_estimate);
    // …but the child never poses as the parent's own last request (that
    // field feeds the context-size estimate for the PARENT's window).
    assert!(state.session.last_token_usage.is_none());
}

#[test]
fn system_prompt_appends_subagent_contract_only_when_flagged() {
    let mut state = fresh_state();
    assert!(
        !system_prompt_for_state(&state).contains("Subagent Contract"),
        "a user-facing session must not carry the subagent contract",
    );
    state.session.is_subagent = true;
    let prompt = system_prompt_for_state(&state);
    assert!(prompt.contains("## Subagent Contract"), "got {prompt}");
    assert!(
        prompt.contains("returned to the parent as the tool result"),
        "the contract must state the report semantics",
    );
    // An agent type's preamble rides after the contract.
    state.session.agent_preamble = Some("## Explore Agent\nRead-only recon.".to_string());
    let prompt = system_prompt_for_state(&state);
    assert!(prompt.contains("## Explore Agent"), "got {prompt}");
    assert!(
        prompt.find("## Subagent Contract") < prompt.find("## Explore Agent"),
        "type preamble must follow the contract",
    );
}

#[test]
fn tick_is_noop() {
    let before = fresh_state();
    let (after, cmds) = update(before.clone(), Msg::Tick);
    assert!(cmds.is_empty());
    assert!(matches!(after.turn, TurnState::Idle));
}

#[test]
fn resize_is_noop() {
    let (state, cmds) = update(
        fresh_state(),
        Msg::Resize {
            width: 80,
            height: 24,
        },
    );
    assert!(cmds.is_empty());
    assert!(matches!(state.turn, TurnState::Idle));
}

#[test]
fn ui_state_default_is_empty() {
    let s = UiState::default();
    assert!(s.input_buffer.is_empty());
    assert!(matches!(s.mode, UiMode::EditingInput));
}
