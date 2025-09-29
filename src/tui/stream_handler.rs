use anyhow::Result;
use tokio::sync::mpsc;

use crate::agents::{self, AgentAction};
use crate::models::MessageRole;
use crate::tui::App;

/// Result of processing streaming chunks
#[derive(Debug, Clone)]
pub enum StreamStatus {
    /// Still streaming, no action needed
    Streaming,
    /// Generation complete with parsed actions
    Complete { actions: Vec<AgentAction> },
    /// Feedback loop complete (ReadFile response)
    FeedbackComplete,
    /// Error occurred during streaming
    Error(String),
}

/// Process streaming chunks from the LLM response channel
///
/// This function consumes all available chunks from the channel,
/// accumulates them into the app.current_response, and detects
/// when the stream is complete (via [DONE] marker).
///
/// Returns StreamStatus indicating whether streaming continues
/// or if actions need to be processed.
pub async fn process_stream_chunks(
    app: &mut App,
    rx: &mut mpsc::Receiver<String>,
) -> Result<StreamStatus> {
    if !app.is_generating {
        return Ok(StreamStatus::Streaming);
    }

    // Process all available messages from the channel
    while let Ok(chunk) = rx.try_recv() {
        if chunk.starts_with("[DONE]:") {
            // Check if this is feedback completion
            let is_feedback_complete = chunk.contains("[FEEDBACK_COMPLETE]");

            // Generation complete
            app.is_generating = false;

            // Clear feedback flags if this was a feedback response
            if is_feedback_complete {
                app.pending_file_read = false;
                app.reading_file_status = None;
                return Ok(StreamStatus::FeedbackComplete);
            }

            // Also clear any lingering file read status on normal completion
            if !app.pending_file_read {
                app.reading_file_status = None;
            }

            // Add the accumulated response from streaming
            if !app.current_response.is_empty() {
                let response_text = app.current_response.clone();
                app.add_message(MessageRole::Assistant, response_text.clone());

                // Parse and execute any actions from the response
                let actions = agents::parse_actions(&response_text);

                // Check if any actions will trigger feedback loops
                let has_feedback_actions = actions
                    .iter()
                    .any(|a| matches!(a, AgentAction::ReadFile { .. }));

                if has_feedback_actions {
                    app.pending_file_read = true;
                    // Extract the model's intent from text before [FILE_READ]
                    app.reading_file_status = extract_reading_intent(&response_text);
                    if app.reading_file_status.is_some() {
                        app.status_timestamp = Some(std::time::Instant::now());
                    }
                }

                // Clear the accumulated response
                app.current_response.clear();

                // Return actions for execution
                return Ok(StreamStatus::Complete { actions });
            }

            return Ok(StreamStatus::Complete { actions: vec![] });
        } else if chunk.starts_with("[ERROR]:") {
            // Error during generation
            let error_msg = chunk.trim_start_matches("[ERROR]:").trim().to_string();
            app.is_generating = false;
            app.current_response.clear();
            app.set_status(format!("[ERROR] {}", error_msg));
            return Ok(StreamStatus::Error(error_msg));
        } else if chunk.starts_with("[HARDWARE_STATS]:") {
            // Handle hardware stats update (don't add to response)
            let json_str = chunk.trim_start_matches("[HARDWARE_STATS]:").trim();
            if let Ok(stats) = serde_json::from_str::<crate::diagnostics::HardwareStats>(json_str) {
                // Update app.hardware_stats directly
                app.hardware_stats = Some(stats);
            }
            continue; // Don't add to response
        } else {
            // Regular streaming chunk - accumulate
            app.current_response.push_str(&chunk);
        }
    }

    Ok(StreamStatus::Streaming)
}

/// Extract the model's reading intent from the text before [FILE_READ]
///
/// This function analyzes the text before a [FILE_READ:] action block
/// to determine what the model intends to do with the file, and generates
/// an appropriate status message.
///
/// Returns Some(status_message) if a FILE_READ action is found, None otherwise.
fn extract_reading_intent(text: &str) -> Option<String> {
    // Find the FILE_READ action block
    if let Some(idx) = text.find("[FILE_READ:") {
        // Get the text before the action
        let before = &text[..idx];

        // Find the file path from the action block
        let file_path = if let Some(end_idx) = text[idx..].find(']') {
            let path_part = &text[idx + 11..idx + end_idx]; // Skip "[FILE_READ:"
            path_part.trim()
        } else {
            "the file"
        };

        // Generate contextual status based on the model's preceding text
        let status = if before.contains("read") || before.contains("Read") {
            format!("Reading {}...", file_path)
        } else if before.contains("check") || before.contains("Check") {
            format!("Checking {}...", file_path)
        } else if before.contains("look") || before.contains("Look") {
            format!("Looking at {}...", file_path)
        } else if before.contains("open") || before.contains("Open") {
            format!("Opening {}...", file_path)
        } else if before.contains("examine") || before.contains("Examine") {
            format!("Examining {}...", file_path)
        } else if before.contains("load") || before.contains("Load") {
            format!("Loading {}...", file_path)
        } else {
            format!("Processing {}...", file_path)
        };

        Some(status)
    } else {
        None
    }
}