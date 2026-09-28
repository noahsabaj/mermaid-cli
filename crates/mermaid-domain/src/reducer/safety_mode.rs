use crate::cmd::Cmd;
use crate::reducer::*;
use crate::state::State;

/// Switch the session safety mode, running whatever the transition needs.
///
/// The ONE entry point for every interactive mode change (Shift+Tab,
/// `/safety <mode>`).
pub fn apply_safety_mode(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    next: mermaid_model::safety::SafetyMode,
) {
    let previous = state.session.safety_mode;
    if previous == next {
        return;
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
