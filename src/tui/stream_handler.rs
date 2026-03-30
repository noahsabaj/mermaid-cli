use anyhow::Result;
use tokio::sync::mpsc;

use super::state::GenerationStatus;
use super::stream_event::StreamEvent;
use crate::models::{ErrorCategory, MessageRole, UserFacingError};
use crate::tui::App;

/// Result of processing streaming chunks
#[derive(Debug, Clone)]
#[must_use]
pub enum StreamStatus {
    /// Still streaming, no action needed
    Streaming,
    /// Generation complete with tool calls from Ollama native function calling
    Complete {
        tool_calls: Vec<crate::models::ToolCall>,
    },
    /// Error occurred during streaming with structured error info
    Error(UserFacingError),
}

/// Process streaming chunks from the LLM response channel
///
/// This function consumes all available events from the channel,
/// accumulates text chunks into the response buffer, and detects
/// when the stream is complete (via StreamEvent::Done).
///
/// Returns StreamStatus indicating whether streaming continues
/// or if actions need to be processed.
pub async fn process_stream_chunks(
    app: &mut App,
    rx: &mut mpsc::Receiver<StreamEvent>,
) -> Result<StreamStatus> {
    if !app.app_state.is_generating() {
        return Ok(StreamStatus::Streaming);
    }

    // Tool calls are accumulated in app.operation_state.accumulated_tool_calls
    // This persists across multiple calls to process_stream_chunks()
    // (tool call events and Done may arrive in separate calls)

    // Process all available events from the channel
    while let Ok(event) = rx.try_recv() {
        match event {
            StreamEvent::Chunk(text) => {
                // Accumulate with size limit (enforced by push_response)
                app.push_response(&text);

                // Transition to Streaming on first content chunk (from Sending or Thinking)
                if app.app_state.generation_status() != Some(GenerationStatus::Streaming) {
                    app.transition_to_streaming();
                }
            },
            StreamEvent::ToolCalls(calls) => {
                // Accumulate in app state so tool calls persist across process_stream_chunks calls
                app.operation_state.accumulated_tool_calls.extend(calls);
            },
            StreamEvent::Done { completion_tokens } => {
                app.set_final_tokens(completion_tokens);

                // Take accumulated tool calls from app state (clears them for next generation)
                let tool_calls = std::mem::take(&mut app.operation_state.accumulated_tool_calls);

                // Commit the accumulated response from streaming (if any)
                let response_text = app.take_response();
                if !response_text.is_empty() {
                    app.add_message(MessageRole::Assistant, response_text);
                }

                // Return tool_calls for execution in the agent loop
                return Ok(StreamStatus::Complete { tool_calls });
            },
            StreamEvent::Error(user_error) => {
                app.clear_response();

                // Check if this is a "does not support thinking" error
                // If so, disable thinking support for this model and inform user
                if user_error.message.contains("does not support thinking") {
                    app.model_state.disable_thinking_support();
                    app.set_status("Model does not support thinking - disabled");
                    app.add_message(
                        MessageRole::System,
                        "This model does not support thinking mode. Thinking has been disabled. Please try your request again.".to_string()
                    );
                    return Ok(StreamStatus::Error(user_error));
                }

                // Check if this is a vision/image-related error
                // If so, mark vision as unsupported for this model and strip images
                // from history to prevent the same error on subsequent requests.
                // Known error strings:
                // - Local Ollama: "does not support images", "is not a multimodal model"
                // - Cloud Ollama: "unsupported content type 'image_url'" (cloud converts
                //   native images field to OpenAI image_url format internally)
                let error_lower = user_error.message.to_lowercase();
                if error_lower.contains("does not support images")
                    || error_lower.contains("images not supported")
                    || error_lower.contains("does not support vision")
                    || error_lower.contains("is not a multimodal model")
                    || error_lower.contains("unsupported content type")
                {
                    app.model_state.vision_supported = Some(false);
                    // Strip images from all messages in history to prevent re-sending
                    for msg in &mut app.session_state.messages {
                        msg.images = None;
                    }
                    app.set_status("Model does not support images - disabled");
                    app.add_message(
                        MessageRole::System,
                        "This model does not support images. Image paste has been disabled for this session.".to_string()
                    );
                    return Ok(StreamStatus::Error(user_error));
                }

                // Display summary in status bar with category-appropriate prefix
                let status_prefix = match user_error.category {
                    ErrorCategory::Connection => "Connection",
                    ErrorCategory::Auth => "Auth",
                    ErrorCategory::Config => "Config",
                    ErrorCategory::NotFound => "Not Found",
                    ErrorCategory::Temporary => "Temporary",
                    ErrorCategory::Internal => "Error",
                };
                app.set_status(format!("[{}] {}", status_prefix, user_error.summary));

                // Add detailed error message to chat with suggestion
                let error_display = format!(
                    "{}\n\nSuggestion: {}",
                    user_error.message, user_error.suggestion
                );
                app.add_message(MessageRole::System, error_display);

                return Ok(StreamStatus::Error(user_error));
            },
        }
    }

    Ok(StreamStatus::Streaming)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_status_variants() {
        // Verify all StreamStatus variants are properly defined

        let streaming = StreamStatus::Streaming;
        assert!(matches!(streaming, StreamStatus::Streaming));

        let complete = StreamStatus::Complete { tool_calls: vec![] };
        assert!(matches!(complete, StreamStatus::Complete { .. }));

        let error = StreamStatus::Error(UserFacingError {
            summary: "Test error".to_string(),
            message: "A test error occurred".to_string(),
            suggestion: "This is just a test".to_string(),
            category: ErrorCategory::Internal,
            recoverable: false,
        });
        assert!(matches!(error, StreamStatus::Error(_)));
    }
}
