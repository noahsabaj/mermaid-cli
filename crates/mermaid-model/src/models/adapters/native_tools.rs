//! Anthropic's own tools: the text editor and bash.
//!
//! Claude is trained on these two definitions, so sending them beats any
//! schema Mermaid writes, and they improve with each model without Mermaid
//! changing. They are declared by `type` alone; the model knows the input
//! shape.
//!
//! Only the wire changes. A native call is rewritten onto the Mermaid tool it
//! stands for as it arrives (`str_replace` becomes `edit_file`, `bash`
//! becomes `execute_command`), so the policy gate, the read-only sandbox,
//! checkpoints, approvals and the transcript all see a tool they already
//! know. The ids of those calls ride on the turn's continuation, and history
//! sends them back in the native form the model wrote.
//!
//! `bash` sits beside `execute_command` rather than replacing it: the native
//! tool has no timeout or background mode, and servers still need both.

use serde_json::{Value, json};

use crate::constants::COMMAND_MAX_TIMEOUT_SECS;
use crate::models::config::NativeTools;
use crate::models::tool_call::FunctionCall;

pub(super) const TEXT_EDITOR_NAME: &str = "str_replace_based_edit_tool";
const TEXT_EDITOR_TYPE: &str = "text_editor_20250728";
pub(super) const BASH_NAME: &str = "bash";
const BASH_TYPE: &str = "bash_20250124";

/// What a refusal of these tools is remembered as, and the words it names
/// them by.
pub(super) const REJECTION: &str = "native_tools";
pub(super) const REJECTION_NAMES: &[&str] = &[
    TEXT_EDITOR_TYPE,
    BASH_TYPE,
    TEXT_EDITOR_NAME,
    "text_editor_",
    "bash_",
];

/// The Mermaid tools the text editor stands in for. All three must be
/// registered: an editor offering `create` to a registry without
/// `write_file` would promise what the harness cannot do.
const EDITOR_TOOLS: [&str; 3] = ["read_file", "write_file", "edit_file"];
const SHELL_TOOL: &str = "execute_command";

/// The native tools one request actually advertises.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Advertised {
    pub(super) text_editor: bool,
    pub(super) bash: bool,
}

impl Advertised {
    /// What `wanted` allows, given the Mermaid tools this request carries.
    pub(super) fn resolve(wanted: NativeTools, registered: &[&str]) -> Self {
        Self {
            text_editor: wanted.text_editor && EDITOR_TOOLS.iter().all(|t| registered.contains(t)),
            bash: wanted.shell && registered.contains(&SHELL_TOOL),
        }
    }

    /// Whether the Mermaid tool `name` goes out as its native stand-in
    /// instead of its own schema.
    pub(super) fn replaces(self, name: &str) -> bool {
        self.text_editor && EDITOR_TOOLS.contains(&name)
    }

    /// The `tools` entries to send.
    pub(super) fn declarations(self) -> Vec<Value> {
        let mut out = Vec::new();
        if self.text_editor {
            out.push(json!({"type": TEXT_EDITOR_TYPE, "name": TEXT_EDITOR_NAME}));
        }
        if self.bash {
            out.push(json!({"type": BASH_TYPE, "name": BASH_NAME}));
        }
        out
    }

    /// `call` in the native form, when it was made natively and that tool is
    /// on offer in this request.
    pub(super) fn to_native(self, call: &FunctionCall) -> Option<(&'static str, Value)> {
        let (name, input) = to_native(call)?;
        let offered = match name {
            TEXT_EDITOR_NAME => self.text_editor,
            _ => self.bash,
        };
        offered.then_some((name, input))
    }
}

/// Rewrite a native call onto the Mermaid tool it stands for. `false` when
/// `call` is not a native call, or is one whose input names no operation
/// Mermaid can run; it is left untouched then, and the registry answers it
/// with the reason.
pub(super) fn canonicalize(call: &mut FunctionCall) -> bool {
    let translated = match call.name.as_str() {
        TEXT_EDITOR_NAME => editor_to_canonical(&call.arguments),
        BASH_NAME => bash_to_canonical(&call.arguments),
        _ => None,
    };
    let Some((name, arguments)) = translated else {
        return false;
    };
    call.name = name.to_string();
    call.arguments = arguments;
    true
}

fn str_field<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

fn editor_to_canonical(input: &Value) -> Option<(&'static str, Value)> {
    let path = str_field(input, "path")?;
    match str_field(input, "command")? {
        "view" => {
            let mut args = json!({"path": path, "line_numbers": true});
            if let Some(range) = input.get("view_range").filter(|r| !r.is_null()) {
                args["view_range"] = range.clone();
            }
            Some(("read_file", args))
        },
        "create" => Some((
            "write_file",
            json!({"path": path, "content": str_field(input, "file_text")?}),
        )),
        "str_replace" => Some((
            "edit_file",
            json!({
                "path": path,
                "target_content": str_field(input, "old_str")?,
                // An absent `new_str` deletes the match.
                "replacement_content": str_field(input, "new_str").unwrap_or_default(),
            }),
        )),
        "insert" => Some((
            "edit_file",
            json!({
                "path": path,
                "insert_line": input.get("insert_line")?.as_u64()?,
                "replacement_content": str_field(input, "insert_text")
                    .or_else(|| str_field(input, "new_str"))?,
            }),
        )),
        _ => None,
    }
}

fn bash_to_canonical(input: &Value) -> Option<(&'static str, Value)> {
    if input.get("restart").and_then(Value::as_bool) == Some(true) {
        return Some((SHELL_TOOL, json!({"restart": true})));
    }
    // The native tool has no timeout of its own, so a build must not die at
    // the short default: it gets the longest foreground run there is.
    Some((
        SHELL_TOOL,
        json!({
            "command": str_field(input, "command")?,
            "timeout": COMMAND_MAX_TIMEOUT_SECS,
        }),
    ))
}

/// Why a native call that reached the tool runner untranslated ran nothing:
/// its input named no operation Mermaid runs. `None` for any other tool.
#[must_use]
pub fn unrunnable(name: &str) -> Option<&'static str> {
    match name {
        TEXT_EDITOR_NAME => Some(
            "nothing ran: Mermaid runs the text editor's view (path, optional \
             view_range), create (path, file_text), str_replace (path, old_str, \
             new_str) and insert (path, insert_line, insert_text) commands, and this \
             call is none of them with its required fields",
        ),
        BASH_NAME => Some("nothing ran: a bash call needs `command`, or `restart: true`"),
        _ => None,
    }
}

/// The native form of a call [`canonicalize`] produced.
fn to_native(call: &FunctionCall) -> Option<(&'static str, Value)> {
    let args = &call.arguments;
    let path = || str_field(args, "path");
    match call.name.as_str() {
        "read_file" => {
            let mut input = json!({"command": "view", "path": path()?});
            if let Some(range) = args.get("view_range") {
                input["view_range"] = range.clone();
            }
            Some((TEXT_EDITOR_NAME, input))
        },
        "write_file" => Some((
            TEXT_EDITOR_NAME,
            json!({
                "command": "create",
                "path": path()?,
                "file_text": str_field(args, "content")?,
            }),
        )),
        "edit_file" => {
            let text = str_field(args, "replacement_content")?;
            let input = match args.get("insert_line") {
                Some(line) => json!({
                    "command": "insert",
                    "path": path()?,
                    "insert_line": line,
                    "insert_text": text,
                }),
                None => json!({
                    "command": "str_replace",
                    "path": path()?,
                    "old_str": str_field(args, "target_content")?,
                    "new_str": text,
                }),
            };
            Some((TEXT_EDITOR_NAME, input))
        },
        SHELL_TOOL if args.get("restart").is_some() => Some((BASH_NAME, json!({"restart": true}))),
        SHELL_TOOL => Some((BASH_NAME, json!({"command": str_field(args, "command")?}))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: Value) -> FunctionCall {
        FunctionCall {
            name: name.to_string(),
            arguments,
        }
    }

    fn all() -> NativeTools {
        NativeTools {
            text_editor: true,
            shell: true,
        }
    }

    const FULL: [&str; 5] = [
        "read_file",
        "write_file",
        "edit_file",
        "apply_patch",
        "execute_command",
    ];

    #[test]
    fn each_native_command_maps_onto_the_tool_it_stands_for() {
        let cases = [
            (
                json!({"command": "view", "path": "a.rs"}),
                "read_file",
                json!({"path": "a.rs", "line_numbers": true}),
            ),
            (
                json!({"command": "view", "path": "a.rs", "view_range": [3, -1]}),
                "read_file",
                json!({"path": "a.rs", "line_numbers": true, "view_range": [3, -1]}),
            ),
            (
                json!({"command": "create", "path": "n.rs", "file_text": "x\n"}),
                "write_file",
                json!({"path": "n.rs", "content": "x\n"}),
            ),
            (
                json!({"command": "str_replace", "path": "a.rs", "old_str": "a", "new_str": "b"}),
                "edit_file",
                json!({"path": "a.rs", "target_content": "a", "replacement_content": "b"}),
            ),
            (
                json!({"command": "str_replace", "path": "a.rs", "old_str": "a"}),
                "edit_file",
                json!({"path": "a.rs", "target_content": "a", "replacement_content": ""}),
            ),
            (
                json!({"command": "insert", "path": "a.rs", "insert_line": 0, "insert_text": "top\n"}),
                "edit_file",
                json!({"path": "a.rs", "insert_line": 0, "replacement_content": "top\n"}),
            ),
        ];
        for (input, name, args) in cases {
            let mut c = call(TEXT_EDITOR_NAME, input.clone());
            assert!(canonicalize(&mut c), "{input}");
            assert_eq!(c.name, name, "{input}");
            assert_eq!(c.arguments, args, "{input}");
        }
    }

    #[test]
    fn bash_runs_through_execute_command_with_the_longest_timeout() {
        let mut c = call(BASH_NAME, json!({"command": "cargo build"}));
        assert!(canonicalize(&mut c));
        assert_eq!(c.name, "execute_command");
        assert_eq!(
            c.arguments,
            json!({"command": "cargo build", "timeout": COMMAND_MAX_TIMEOUT_SECS})
        );
        let mut restart = call(BASH_NAME, json!({"restart": true}));
        assert!(canonicalize(&mut restart));
        assert_eq!(restart.arguments, json!({"restart": true}));
    }

    #[test]
    fn a_call_mermaid_cannot_run_is_left_for_the_registry_to_answer() {
        for input in [
            json!({"command": "undo_edit", "path": "a.rs"}),
            json!({"command": "create", "path": "a.rs"}),
            json!({"command": "view"}),
        ] {
            let mut c = call(TEXT_EDITOR_NAME, input.clone());
            assert!(!canonicalize(&mut c), "{input}");
            assert_eq!(c.name, TEXT_EDITOR_NAME);
            assert_eq!(c.arguments, input);
        }
        let mut other = call("read_file", json!({"path": "a"}));
        assert!(!canonicalize(&mut other));
        assert_eq!(other.name, "read_file");
    }

    #[test]
    fn history_gets_back_exactly_what_the_model_sent() {
        let adv = Advertised::resolve(all(), &FULL);
        for (name, input) in [
            (TEXT_EDITOR_NAME, json!({"command": "view", "path": "a.rs"})),
            (
                TEXT_EDITOR_NAME,
                json!({"command": "view", "path": "a.rs", "view_range": [1, 9]}),
            ),
            (
                TEXT_EDITOR_NAME,
                json!({"command": "create", "path": "n.rs", "file_text": "x"}),
            ),
            (
                TEXT_EDITOR_NAME,
                json!({"command": "str_replace", "path": "a", "old_str": "o", "new_str": "n"}),
            ),
            (
                TEXT_EDITOR_NAME,
                json!({"command": "insert", "path": "a", "insert_line": 4, "insert_text": "t"}),
            ),
            (BASH_NAME, json!({"command": "ls"})),
            (BASH_NAME, json!({"restart": true})),
        ] {
            let mut c = call(name, input.clone());
            assert!(canonicalize(&mut c));
            assert_eq!(adv.to_native(&c), Some((name, input)));
        }
    }

    #[test]
    fn the_editor_needs_every_tool_it_stands_for() {
        let adv = Advertised::resolve(all(), &["read_file", "execute_command"]);
        assert!(!adv.text_editor, "a read-only toolset cannot offer create");
        assert!(adv.bash);
        assert!(!adv.replaces("read_file"));
        assert_eq!(adv.declarations().len(), 1);

        let none = Advertised::resolve(NativeTools::default(), &FULL);
        assert_eq!(none, Advertised::default());
        assert!(none.declarations().is_empty());
    }

    #[test]
    fn declarations_carry_only_type_and_name() {
        let adv = Advertised::resolve(all(), &FULL);
        assert_eq!(
            adv.declarations(),
            vec![
                json!({"type": "text_editor_20250728", "name": "str_replace_based_edit_tool"}),
                json!({"type": "bash_20250124", "name": "bash"}),
            ]
        );
        assert!(adv.replaces("edit_file"));
        assert!(!adv.replaces("execute_command"), "bash sits beside it");
        assert!(!adv.replaces("apply_patch"));
    }

    #[test]
    fn a_call_is_replayed_natively_only_while_its_tool_is_offered() {
        let mut c = call(BASH_NAME, json!({"command": "ls"}));
        assert!(canonicalize(&mut c));
        let editor_only = Advertised::resolve(
            NativeTools {
                text_editor: true,
                shell: false,
            },
            &FULL,
        );
        assert_eq!(editor_only.to_native(&c), None);
    }
}
