//! OpenAI's `computer` tool on the Responses API, mapped onto Mermaid's
//! `computer` tool.
//!
//! One `computer_call` carries a list of actions (`click`, `type`,
//! `keypress`, ...) in screenshot pixels and wants one screenshot back. It
//! runs as one call of Mermaid's tool with the `batch` action, which runs the
//! actions in order and returns the screen after them, so the policy gate,
//! the pointer watch and the transcript see the tool they know. The call is
//! replayed in the native form from the turn's continuation, and its result
//! goes back as a `computer_call_output` carrying the screenshot.
//!
//! The API's `pending_safety_checks` ride on the batch as `warnings`, which
//! the gate shows with the actions; replay acknowledges them, since the gate
//! saw them before anything ran.

use serde_json::{Value, json};

use super::computer_toolset::TOOL;
use crate::models::tool_call::FunctionCall;
use crate::models::types::ChatMessage;

/// The `tools` entry, and what a refusal of it is remembered as.
pub(super) const TOOL_TYPE: &str = "computer";
pub(super) const REJECTION: &str = "native_computer";
/// The output item type a call arrives as.
pub(super) const CALL_TYPE: &str = "computer_call";

/// Seconds a `wait` waits: the action names no duration.
const WAIT_SECS: f64 = 2.0;
/// Pixels per wheel click: OpenAI scrolls in pixels, Mermaid in clicks.
const PIXELS_PER_CLICK: i64 = 100;
/// What goes back when a result has no screenshot (the gate refused the
/// batch before anything ran, and no screen was seen yet): one black pixel,
/// since the output must be a picture. The result's text follows it.
const NO_SCREEN: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAElFTkSuQmCC";

/// A `computer_call` as one `batch` call of Mermaid's `computer` tool.
pub(super) fn canonicalize(item: &Value) -> FunctionCall {
    let actions: Vec<Value> = match item.get("actions").and_then(Value::as_array) {
        Some(list) => list.iter().flat_map(to_mermaid).collect(),
        None => item.get("action").map(to_mermaid).unwrap_or_default(),
    };
    let mut arguments = json!({"action": "batch", "actions": actions});
    let warnings: Vec<&str> = item
        .get("pending_safety_checks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|check| {
            check
                .get("message")
                .or_else(|| check.get("code"))
                .and_then(Value::as_str)
        })
        .collect();
    if !warnings.is_empty() {
        arguments["warnings"] = json!(warnings);
    }
    FunctionCall {
        name: TOOL.to_string(),
        arguments,
    }
}

/// One OpenAI action as Mermaid actions: one, except for a drag along a
/// path of more than two points. An unknown type goes through by name, and
/// Mermaid's tool refuses it, which stops the batch there.
fn to_mermaid(action: &Value) -> Vec<Value> {
    let int = |key: &str| action.get(key).and_then(Value::as_i64);
    let at = |name: &str| {
        let mut out = json!({"action": name});
        if let (Some(x), Some(y)) = (int("x"), int("y")) {
            out["coordinate"] = json!([x, y]);
        }
        out
    };
    let holding = |mut out: Value| {
        if let Some(chord) = chord(action.get("keys")) {
            out["text"] = json!(chord);
        }
        out
    };
    let kind = action
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match kind {
        "screenshot" => vec![json!({"action": "screenshot"})],
        "click" => match action.get("button").and_then(Value::as_str) {
            Some("back") => vec![key(BACK)],
            Some("forward") => vec![key(FORWARD)],
            Some("right") => vec![holding(at("right_click"))],
            Some("wheel") => vec![holding(at("middle_click"))],
            _ => vec![holding(at("left_click"))],
        },
        "double_click" => vec![holding(at("double_click"))],
        "move" => vec![at("mouse_move")],
        "drag" => drag(action, holding),
        "keypress" => vec![json!({"action": "key", "text": chord(action.get("keys"))})],
        "type" => vec![json!({"action": "type", "text": action.get("text")})],
        "scroll" => scroll(action, holding),
        "wait" => vec![json!({"action": "wait", "duration": WAIT_SECS})],
        other => vec![json!({"action": other})],
    }
}

/// The mouse's back and forward buttons are the browser's history keys.
#[cfg(target_os = "macos")]
pub(super) const BACK: &str = "cmd+bracketleft";
#[cfg(target_os = "macos")]
pub(super) const FORWARD: &str = "cmd+bracketright";
#[cfg(not(target_os = "macos"))]
pub(super) const BACK: &str = "alt+Left";
#[cfg(not(target_os = "macos"))]
pub(super) const FORWARD: &str = "alt+Right";

fn key(chord: &str) -> Value {
    json!({"action": "key", "text": chord})
}

/// OpenAI's key list as one chord: `["CTRL", "S"]` is `CTRL+S`. Mermaid
/// reads key names case-insensitively.
fn chord(keys: Option<&Value>) -> Option<String> {
    let keys: Vec<&str> = keys
        .and_then(Value::as_array)?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    (!keys.is_empty()).then(|| keys.join("+"))
}

/// A drag along `path`. Two points are one `left_click_drag`; a longer path
/// presses at the first point, moves through the rest and releases at the
/// last, without held keys.
fn drag(action: &Value, holding: impl Fn(Value) -> Value) -> Vec<Value> {
    let path: Vec<[i64; 2]> = action
        .get("path")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| Some([p.get("x")?.as_i64()?, p.get("y")?.as_i64()?]))
        .collect();
    match path.as_slice() {
        [from, to] => vec![holding(json!({
            "action": "left_click_drag",
            "start_coordinate": from,
            "coordinate": to,
        }))],
        [first, middle @ .., last] if !middle.is_empty() => {
            let mut out = vec![json!({"action": "left_mouse_down", "coordinate": first})];
            out.extend(
                middle
                    .iter()
                    .map(|p| json!({"action": "mouse_move", "coordinate": p})),
            );
            out.push(json!({"action": "left_mouse_up", "coordinate": last}));
            out
        },
        // Mermaid's tool refuses a drag with no path and says why.
        _ => vec![json!({"action": "left_click_drag"})],
    }
}

/// A scroll by `scroll_x`/`scroll_y` pixels as wheel clicks, one action per
/// axis that moves.
fn scroll(action: &Value, holding: impl Fn(Value) -> Value) -> Vec<Value> {
    let int = |key: &str| action.get(key).and_then(Value::as_i64).unwrap_or(0);
    let mut out = Vec::new();
    for (pixels, back, forth) in [
        (int("scroll_y"), "up", "down"),
        (int("scroll_x"), "left", "right"),
    ] {
        if pixels == 0 {
            continue;
        }
        let clicks = ((pixels.abs() + PIXELS_PER_CLICK / 2) / PIXELS_PER_CLICK).max(1);
        let mut one = json!({
            "action": "scroll",
            "scroll_direction": if pixels < 0 { back } else { forth },
            "scroll_amount": clicks,
        });
        if let (Some(x), Some(y)) = (
            action.get("x").and_then(Value::as_i64),
            action.get("y").and_then(Value::as_i64),
        ) {
            one["coordinate"] = json!([x, y]);
        }
        out.push(holding(one));
    }
    if out.is_empty() {
        // A scroll of nothing still moves the pointer where it points.
        out.push(json!({"action": "scroll", "scroll_direction": "down", "scroll_amount": 0}));
    }
    out
}

/// The `computer_call_output` answering `call` with `result`, and the text
/// the model must also read: the result's text when it has no screenshot or
/// reports a failure. The output itself can carry only the picture.
pub(super) fn output(call: &Value, result: &ChatMessage) -> (Value, Option<String>) {
    let image = result.images.iter().flatten().next();
    let data = image.map_or(NO_SCREEN, String::as_str);
    let media_type = crate::utils::base64_image_media_type(data);
    let mut out = json!({
        "type": "computer_call_output",
        "call_id": call.get("call_id").cloned().unwrap_or_default(),
        "output": {
            "type": "computer_screenshot",
            "image_url": format!("data:{media_type};base64,{data}"),
        },
    });
    if let Some(checks) = call
        .get("pending_safety_checks")
        .and_then(Value::as_array)
        .filter(|checks| !checks.is_empty())
    {
        out["acknowledged_safety_checks"] = json!(checks);
    }
    let note =
        (image.is_none() || result.content.starts_with("Error")).then(|| result.content.clone());
    (out, note)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(actions: Value) -> Value {
        canonicalize(&json!({
            "type": "computer_call", "call_id": "call_1", "pending_safety_checks": [],
            "actions": actions,
        }))
        .arguments
    }

    #[test]
    fn each_action_becomes_mermaids() {
        let args = batch(json!([
            {"type": "screenshot"},
            {"type": "click", "button": "left", "x": 10, "y": 20, "keys": ["SHIFT"]},
            {"type": "click", "button": "right", "x": 1, "y": 2},
            {"type": "click", "button": "wheel", "x": 1, "y": 2},
            {"type": "double_click", "x": 3, "y": 4},
            {"type": "move", "x": 5, "y": 6},
            {"type": "keypress", "keys": ["CTRL", "S"]},
            {"type": "type", "text": "hello"},
            {"type": "wait"},
        ]));
        assert_eq!(args["action"], "batch");
        assert_eq!(
            args["actions"],
            json!([
                {"action": "screenshot"},
                {"action": "left_click", "coordinate": [10, 20], "text": "SHIFT"},
                {"action": "right_click", "coordinate": [1, 2]},
                {"action": "middle_click", "coordinate": [1, 2]},
                {"action": "double_click", "coordinate": [3, 4]},
                {"action": "mouse_move", "coordinate": [5, 6]},
                {"action": "key", "text": "CTRL+S"},
                {"action": "type", "text": "hello"},
                {"action": "wait", "duration": WAIT_SECS},
            ])
        );
        assert!(args.get("warnings").is_none());
    }

    #[test]
    fn a_single_action_call_is_a_batch_of_one() {
        let call = canonicalize(&json!({
            "type": "computer_call", "call_id": "c", "action": {"type": "screenshot"},
        }));
        assert_eq!(call.name, "computer");
        assert_eq!(call.arguments["actions"], json!([{"action": "screenshot"}]));
    }

    #[test]
    fn a_drag_follows_its_path() {
        let two = batch(
            json!([{"type": "drag", "path": [{"x": 1, "y": 2}, {"x": 3, "y": 4}],
                                "keys": ["ALT"]}]),
        );
        assert_eq!(
            two["actions"],
            json!([{"action": "left_click_drag", "start_coordinate": [1, 2],
                    "coordinate": [3, 4], "text": "ALT"}])
        );
        let three = batch(json!([{"type": "drag",
            "path": [{"x": 1, "y": 2}, {"x": 5, "y": 5}, {"x": 3, "y": 4}]}]));
        assert_eq!(
            three["actions"],
            json!([
                {"action": "left_mouse_down", "coordinate": [1, 2]},
                {"action": "mouse_move", "coordinate": [5, 5]},
                {"action": "left_mouse_up", "coordinate": [3, 4]},
            ])
        );
        let none = batch(json!([{"type": "drag", "path": []}]));
        assert_eq!(none["actions"], json!([{"action": "left_click_drag"}]));
    }

    #[test]
    fn a_scroll_in_pixels_is_wheel_clicks_per_axis() {
        let args = batch(json!([
            {"type": "scroll", "x": 100, "y": 200, "scroll_x": 0, "scroll_y": 300},
            {"type": "scroll", "x": 100, "y": 200, "scroll_x": -40, "scroll_y": -260},
        ]));
        assert_eq!(
            args["actions"],
            json!([
                {"action": "scroll", "scroll_direction": "down", "scroll_amount": 3,
                 "coordinate": [100, 200]},
                {"action": "scroll", "scroll_direction": "up", "scroll_amount": 3,
                 "coordinate": [100, 200]},
                {"action": "scroll", "scroll_direction": "left", "scroll_amount": 1,
                 "coordinate": [100, 200]},
            ])
        );
    }

    #[test]
    fn safety_checks_ride_as_warnings_and_are_acknowledged() {
        let item = json!({
            "type": "computer_call", "call_id": "call_9", "id": "cu_9", "status": "completed",
            "pending_safety_checks": [
                {"id": "sc_1", "code": "malicious_instructions",
                 "message": "The screen may contain instructions to the agent."},
                {"id": "sc_2", "code": "sensitive_domain"},
            ],
            "actions": [{"type": "click", "button": "left", "x": 1, "y": 1}],
        });
        let call = canonicalize(&item);
        assert_eq!(
            call.arguments["warnings"],
            json!([
                "The screen may contain instructions to the agent.",
                "sensitive_domain"
            ])
        );
        let result = ChatMessage::tool("call_9", "computer", "Ran 2 actions.")
            .with_images(vec![NO_SCREEN.to_string()]);
        let (out, note) = output(&item, &result);
        assert_eq!(
            out["acknowledged_safety_checks"],
            item["pending_safety_checks"]
        );
        assert!(note.is_none(), "a picture of a success says it all");
    }

    #[test]
    fn the_output_is_the_screenshot_and_a_failure_is_also_told_in_words() {
        let call =
            json!({"type": "computer_call", "call_id": "call_1", "pending_safety_checks": []});
        let ok = ChatMessage::tool("call_1", "computer", "Ran 1 action.")
            .with_images(vec!["iVBORw0KGgoAAAANSUhEUg==".to_string()]);
        let (out, note) = output(&call, &ok);
        assert_eq!(
            out,
            json!({"type": "computer_call_output", "call_id": "call_1", "output": {
                "type": "computer_screenshot",
                "image_url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==",
            }})
        );
        assert!(note.is_none());

        let failed = ChatMessage::tool("call_1", "computer", "Error: the user moved the mouse")
            .with_images(vec!["iVBORw0KGgoAAAANSUhEUg==".to_string()]);
        assert_eq!(
            output(&call, &failed).1.as_deref(),
            Some("Error: the user moved the mouse")
        );

        let blocked = ChatMessage::tool("call_1", "computer", "Error: denied by the user");
        let (out, note) = output(&call, &blocked);
        assert_eq!(
            out["output"]["image_url"],
            format!("data:image/png;base64,{NO_SCREEN}")
        );
        assert_eq!(note.as_deref(), Some("Error: denied by the user"));
    }
}
