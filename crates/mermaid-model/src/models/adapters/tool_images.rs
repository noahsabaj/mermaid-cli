//! Images that tool results return, for providers whose tool messages
//! carry text only.
//!
//! Anthropic puts an image inside the `tool_result` itself. OpenAI's Chat
//! Completions, Ollama and Responses-style APIs take images only on a user
//! turn, so those adapters send the images of a run of tool results as one
//! user turn right after it, each labelled with the call that returned it.

use crate::models::types::{ChatMessage, MessageRole};

/// One image a tool call returned.
pub(super) struct ToolImage<'a> {
    pub(super) call_id: &'a str,
    pub(super) data: &'a str,
}

impl ToolImage<'_> {
    /// The text that precedes the image in the user turn.
    pub(super) fn label(&self) -> String {
        format!("Image returned by tool call {}:", self.call_id)
    }
}

/// When `messages[idx]` ends a run of tool results, every image that run
/// returned, in order; otherwise none.
pub(super) fn images_after_tool_run(messages: &[ChatMessage], idx: usize) -> Vec<ToolImage<'_>> {
    let is_tool = |m: &ChatMessage| m.role == MessageRole::Tool;
    if !messages.get(idx).is_some_and(is_tool) || messages.get(idx + 1).is_some_and(is_tool) {
        return Vec::new();
    }
    let start = messages[..idx]
        .iter()
        .rposition(|m| !is_tool(m))
        .map_or(0, |pos| pos + 1);
    messages[start..=idx]
        .iter()
        .flat_map(|m| {
            let call_id = m.tool_call_id.as_deref().unwrap_or_default();
            m.images
                .iter()
                .flatten()
                .map(move |data| ToolImage { call_id, data })
        })
        .collect()
}

/// A JPEG header, base64: what a `read_file` of a photo returns.
#[cfg(test)]
pub(super) const JPEG_B64: &str = "/9j/4AAQSkZJRgABAQA=";

/// A tool loop whose first result returned a picture: user, assistant
/// calling `c1` and `c2`, both results (`c1` with [`JPEG_B64`]), assistant.
#[cfg(test)]
pub(super) fn tool_loop_with_image() -> Vec<ChatMessage> {
    use crate::models::tool_call::{FunctionCall, ToolCall};
    let call = |id: &str| ToolCall {
        id: Some(id.to_string()),
        function: FunctionCall {
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "photo.jpg"}),
        },
    };
    vec![
        ChatMessage::user("what is in photo.jpg?"),
        ChatMessage::assistant("Reading it.").with_tool_calls(vec![call("c1"), call("c2")]),
        ChatMessage::tool("c1", "read_file", "[image/jpeg, 14 bytes]")
            .with_images(vec![JPEG_B64.to_string()]),
        ChatMessage::tool("c2", "read_file", "plain text"),
        ChatMessage::assistant("A cat."),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(id: &str, images: &[&str]) -> ChatMessage {
        let msg = ChatMessage::tool(id.to_string(), "read_file".to_string(), "ok".to_string());
        if images.is_empty() {
            msg
        } else {
            msg.with_images(images.iter().map(ToString::to_string).collect())
        }
    }

    fn collected(messages: &[ChatMessage], idx: usize) -> Vec<(String, String)> {
        images_after_tool_run(messages, idx)
            .into_iter()
            .map(|image| (image.call_id.to_string(), image.data.to_string()))
            .collect()
    }

    #[test]
    fn the_last_result_of_a_run_carries_the_whole_run() {
        let messages = vec![
            ChatMessage::user("look"),
            tool("a", &["IMG1"]),
            tool("b", &[]),
            tool("c", &["IMG2", "IMG3"]),
            ChatMessage::assistant("seen"),
        ];
        assert!(collected(&messages, 0).is_empty());
        assert!(collected(&messages, 1).is_empty(), "the run goes on");
        assert!(collected(&messages, 2).is_empty(), "the run goes on");
        assert_eq!(
            collected(&messages, 3),
            vec![
                ("a".to_string(), "IMG1".to_string()),
                ("c".to_string(), "IMG2".to_string()),
                ("c".to_string(), "IMG3".to_string()),
            ]
        );
        assert!(collected(&messages, 4).is_empty());
    }

    #[test]
    fn runs_are_separate() {
        let messages = vec![
            tool("a", &["IMG1"]),
            ChatMessage::assistant("next"),
            tool("b", &["IMG2"]),
        ];
        assert_eq!(
            collected(&messages, 0),
            vec![("a".to_string(), "IMG1".to_string())]
        );
        assert_eq!(
            collected(&messages, 2),
            vec![("b".to_string(), "IMG2".to_string())]
        );
    }

    #[test]
    fn a_label_names_its_call() {
        let image = ToolImage {
            call_id: "call_7",
            data: "IMG",
        };
        assert_eq!(image.label(), "Image returned by tool call call_7:");
    }
}
