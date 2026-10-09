//! Gemini's computer use tool, mapped onto Mermaid's `computer` tool.
//!
//! Gemini 3 models are trained on a `computer_use` tool whose actions arrive
//! as plain function calls (`click`, `type`, `hotkey`, ...) with coordinates
//! out of 1000 across the screen, and each wants the screen back. Each call
//! runs as one `batch` call of Mermaid's tool with `scale: 1000`, which
//! turns the coordinates into pixels, runs the actions and returns a
//! screenshot, so the policy gate, the pointer watch and the transcript see
//! the tool they know. The call goes back as the model wrote it, from the
//! turn's continuation, and its result as a function response carrying the
//! screenshot.
//!
//! The functions Mermaid's tool cannot honour (`navigate`, and the separate
//! `key_down`/`key_up`) are excluded in the declaration. A call's
//! `safety_decision` asking for confirmation rides on the batch as a
//! warning the gate shows, and the response acknowledges it; one that blocks
//! the action stops the batch before anything runs.

use serde_json::{Value, json};

use super::computer_toolset::TOOL;
use super::openai_computer::{BACK, FORWARD};
use crate::models::tool_call::FunctionCall;
use crate::models::types::{ChatMessage, GeminiNativeCall};

/// What a refusal of the tool is remembered as, and the words it is named by
/// in an error (lowercased).
pub(super) const REJECTION: &str = "native_computer";
pub(super) const REJECTION_NAMES: &[&str] = &["computer_use", "computeruse", "computer use"];

/// The coordinate range: Gemini's points are out of 1000 across the screen.
const SCALE: u32 = 1000;
/// Pixels per wheel click, for `magnitude_in_pixels`.
const PIXELS_PER_CLICK: i64 = 100;

/// The functions the declaration leaves out: Mermaid's tool drives the
/// screen, not a browser's address bar, and it presses keys as chords.
const EXCLUDED: [&str; 3] = ["navigate", "key_down", "key_up"];

/// The `tools` entry.
pub(super) fn declaration() -> Value {
    json!({"computer_use": {
        "environment": "ENVIRONMENT_DESKTOP",
        "enable_prompt_injection_detection": true,
        "excluded_predefined_functions": EXCLUDED,
    }})
}

/// A computer function call as one `batch` call of Mermaid's `computer`
/// tool. `None` for a name that is no computer function.
pub(super) fn canonicalize(name: &str, args: &Value) -> Option<FunctionCall> {
    let actions = to_mermaid(name, args)?;
    let mut arguments = json!({"action": "batch", "scale": SCALE, "actions": actions});
    let safety = args.get("safety_decision");
    let explanation = safety
        .and_then(|d| d.get("explanation"))
        .and_then(Value::as_str)
        .unwrap_or("no explanation given");
    match safety
        .and_then(|d| d.get("decision"))
        .and_then(Value::as_str)
    {
        Some("require_confirmation") => arguments["warnings"] = json!([explanation]),
        Some("blocked") => arguments["refused"] = json!(explanation),
        _ => {},
    }
    Some(FunctionCall {
        name: TOOL.to_string(),
        arguments,
    })
}

fn to_mermaid(name: &str, args: &Value) -> Option<Vec<Value>> {
    let int = |key: &str| args.get(key).and_then(Value::as_i64);
    let at = |action: &str| {
        let mut out = json!({"action": action});
        if let (Some(x), Some(y)) = (int("x"), int("y")) {
            out["coordinate"] = json!([x, y]);
        }
        out
    };
    let text = |key: &str| args.get(key).cloned().unwrap_or(Value::Null);
    Some(match name {
        "click" => vec![at("left_click")],
        "double_click" => vec![at("double_click")],
        "triple_click" => vec![at("triple_click")],
        "middle_click" => vec![at("middle_click")],
        "right_click" => vec![at("right_click")],
        "mouse_down" => vec![at("left_mouse_down")],
        "mouse_up" => vec![at("left_mouse_up")],
        "move" => vec![at("mouse_move")],
        "type" => {
            let mut out = vec![json!({"action": "type", "text": text("text")})];
            if args.get("press_enter").and_then(Value::as_bool) == Some(true) {
                out.push(json!({"action": "key", "text": "Return"}));
            }
            out
        },
        "drag_and_drop" => vec![json!({
            "action": "left_click_drag",
            "start_coordinate": [int("start_x"), int("start_y")],
            "coordinate": [int("end_x"), int("end_y")],
        })],
        "wait" => vec![json!({
            "action": "wait",
            "duration": args.get("seconds").and_then(Value::as_f64).unwrap_or(1.0),
        })],
        "press_key" => vec![json!({"action": "key", "text": text("key")})],
        "hotkey" => {
            let keys: Vec<&str> = args
                .get("keys")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            vec![json!({"action": "key", "text": keys.join("+")})]
        },
        // The batch ends with a screenshot anyway.
        "take_screenshot" => Vec::new(),
        "scroll" => {
            let pixels = int("magnitude_in_pixels").unwrap_or(300).abs();
            let mut out = at("scroll");
            out["scroll_direction"] = text("direction");
            out["scroll_amount"] =
                json!(((pixels + PIXELS_PER_CLICK / 2) / PIXELS_PER_CLICK).max(1));
            vec![out]
        },
        "go_back" => vec![json!({"action": "key", "text": BACK})],
        "go_forward" => vec![json!({"action": "key", "text": FORWARD})],
        _ => return None,
    })
}

/// The function response answering `call` with `result`: the result's text,
/// an acknowledgement of a safety decision the gate saw, and the screenshot
/// as an inline part.
pub(super) fn response(call: &GeminiNativeCall, result: &ChatMessage) -> Value {
    let mut body = json!({"result": result.content});
    let confirmed = call
        .args
        .pointer("/safety_decision/decision")
        .and_then(Value::as_str)
        == Some("require_confirmation");
    if confirmed {
        body["safety_acknowledgement"] = json!("true");
    }
    let mut out = json!({"functionResponse": {"name": call.name, "response": body}});
    let parts: Vec<Value> = result
        .images
        .iter()
        .flatten()
        .map(|data| {
            json!({"inlineData": {
                "mimeType": crate::utils::base64_image_media_type(data),
                "data": data,
            }})
        })
        .collect();
    if !parts.is_empty() {
        out["functionResponse"]["parts"] = json!(parts);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actions(name: &str, args: Value) -> Value {
        canonicalize(name, &args).expect(name).arguments["actions"].clone()
    }

    #[test]
    fn each_function_becomes_mermaids_actions_out_of_1000() {
        let call = canonicalize("click", &json!({"x": 500, "y": 250, "intent": "OK"})).unwrap();
        assert_eq!(call.name, "computer");
        assert_eq!(
            call.arguments,
            json!({"action": "batch", "scale": 1000,
                   "actions": [{"action": "left_click", "coordinate": [500, 250]}]})
        );
        assert_eq!(
            actions("type", json!({"text": "8080", "press_enter": true})),
            json!([{"action": "type", "text": "8080"}, {"action": "key", "text": "Return"}])
        );
        assert_eq!(
            actions(
                "drag_and_drop",
                json!({"start_x": 1, "start_y": 2, "end_x": 3, "end_y": 4})
            ),
            json!([{"action": "left_click_drag", "start_coordinate": [1, 2], "coordinate": [3, 4]}])
        );
        assert_eq!(
            actions("hotkey", json!({"keys": ["Control", "s"]})),
            json!([{"action": "key", "text": "Control+s"}])
        );
        assert_eq!(
            actions("scroll", json!({"x": 10, "y": 20, "direction": "down"})),
            json!([{"action": "scroll", "coordinate": [10, 20], "scroll_direction": "down",
                    "scroll_amount": 3}])
        );
        assert_eq!(
            actions("wait", json!({})),
            json!([{"action": "wait", "duration": 1.0}])
        );
        assert_eq!(actions("take_screenshot", json!({})), json!([]));
        assert!(canonicalize("read_file", &json!({})).is_none());
        for excluded in EXCLUDED {
            assert!(canonicalize(excluded, &json!({})).is_none(), "{excluded}");
        }
    }

    #[test]
    fn a_safety_decision_warns_or_refuses() {
        let confirm = canonicalize(
            "click",
            &json!({"x": 1, "y": 1, "safety_decision": {
                "decision": "require_confirmation", "explanation": "Accepts the cookie terms."}}),
        )
        .unwrap();
        assert_eq!(
            confirm.arguments["warnings"],
            json!(["Accepts the cookie terms."])
        );
        let blocked = canonicalize(
            "click",
            &json!({"x": 1, "y": 1, "safety_decision": {
                "decision": "blocked", "explanation": "Buys something."}}),
        )
        .unwrap();
        assert_eq!(blocked.arguments["refused"], "Buys something.");
    }

    #[test]
    fn the_response_carries_the_screenshot_and_the_acknowledgement() {
        let call = GeminiNativeCall {
            call_id: "call_0".to_string(),
            name: "click".to_string(),
            args: json!({"x": 1, "y": 1, "safety_decision": {"decision": "require_confirmation"}}),
        };
        let result = ChatMessage::tool("call_0", "computer", "Ran 1 action.")
            .with_images(vec!["iVBORw0KGgoAAAANSUhEUg==".to_string()]);
        assert_eq!(
            response(&call, &result),
            json!({"functionResponse": {
                "name": "click",
                "response": {"result": "Ran 1 action.", "safety_acknowledgement": "true"},
                "parts": [{"inlineData": {"mimeType": "image/png",
                                          "data": "iVBORw0KGgoAAAANSUhEUg=="}}],
            }})
        );
    }
}
