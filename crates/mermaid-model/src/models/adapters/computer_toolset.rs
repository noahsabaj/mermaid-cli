//! Anthropic's computer toolset, mapped onto Mermaid's `computer` tool.
//!
//! Claude is trained on `computer_toolset_20260801`: one `tools` entry,
//! seventeen member tools (`screenshot`, `left_click`, `type`, `zoom`, ...).
//! Each call is a `tool_use` named for its member and tagged
//! `toolset_name: "computer"`. Mermaid's own `computer` tool takes the same
//! members as an `action` field, so a native call is rewritten onto it as it
//! arrives and the gate, approvals and transcript see one tool. History
//! sends it back as the model wrote it, and each result echoes the toolset
//! name, which the API requires.

use serde_json::{Map, Value, json};

use crate::models::tool_call::FunctionCall;

/// The `tools` entry type.
pub(super) const TOOLSET_TYPE: &str = "computer_toolset_20260801";
/// The `toolset_name` on each member call and on each result.
pub(super) const TOOLSET_NAME: &str = "computer";
/// The Mermaid tool the toolset stands in for.
pub const TOOL: &str = "computer";
/// The result of each action in a batch after one that failed. The actions
/// run in order, and the rest are not run.
pub const HALTED: &str = "Not executed: an earlier computer action in this turn failed.";

/// What a refusal of the toolset is remembered as, and the words it names
/// it by.
pub(super) const REJECTION: &str = "computer_toolset";
pub(super) const REJECTION_NAMES: &[&str] = &[TOOLSET_TYPE, "computer_toolset"];

/// The member tools, which are also the `action` values of Mermaid's tool.
pub const MEMBERS: [&str; 17] = [
    "screenshot",
    "zoom",
    "left_click",
    "right_click",
    "middle_click",
    "double_click",
    "triple_click",
    "left_click_drag",
    "mouse_move",
    "left_mouse_down",
    "left_mouse_up",
    "cursor_position",
    "scroll",
    "type",
    "key",
    "hold_key",
    "wait",
];

/// The declaration: the type alone, with every member on.
pub(super) fn declaration() -> Value {
    json!({"type": TOOLSET_TYPE})
}

/// Rewrite a member call onto Mermaid's `computer` tool: the member becomes
/// the `action`, the input's fields ride beside it. `None` for a name that is
/// no member.
pub(super) fn canonicalize(member: &str, input: &Value) -> Option<FunctionCall> {
    if !MEMBERS.contains(&member) {
        return None;
    }
    let mut arguments = input.as_object().cloned().unwrap_or_default();
    arguments.insert("action".to_string(), json!(member));
    Some(FunctionCall {
        name: TOOL.to_string(),
        arguments: Value::Object(arguments),
    })
}

/// The member call `call` was rewritten from: its name and input.
pub(super) fn to_native(call: &FunctionCall) -> Option<(String, Value)> {
    if call.name != TOOL {
        return None;
    }
    let mut input: Map<String, Value> = call.arguments.as_object()?.clone();
    let member = input.remove("action")?.as_str()?.to_string();
    MEMBERS
        .contains(&member.as_str())
        .then_some((member, Value::Object(input)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_member_call_becomes_an_action_and_comes_back_as_written() {
        for (member, input) in [
            ("screenshot", json!({})),
            (
                "left_click",
                json!({"coordinate": [10, 20], "text": "shift"}),
            ),
            ("type", json!({"text": "hello"})),
            ("zoom", json!({"region": [0, 0, 100, 50]})),
            ("key", json!({"text": "ctrl+s", "repeat": 2})),
        ] {
            let call = canonicalize(member, &input).expect(member);
            assert_eq!(call.name, "computer");
            assert_eq!(call.arguments["action"], member);
            assert_eq!(to_native(&call), Some((member.to_string(), input)));
        }
    }

    #[test]
    fn a_name_that_is_no_member_is_left_alone() {
        assert!(canonicalize("read_file", &json!({})).is_none());
        let other = FunctionCall {
            name: "computer".to_string(),
            arguments: json!({"action": "teleport"}),
        };
        assert!(to_native(&other).is_none());
        let not_computer = FunctionCall {
            name: "read_file".to_string(),
            arguments: json!({"action": "screenshot"}),
        };
        assert!(to_native(&not_computer).is_none());
    }
}
