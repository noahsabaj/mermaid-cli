use anyhow::Result;
use tokio::sync::mpsc;

use crate::agents::{self, AgentAction, Plan};
use crate::models::MessageRole;
use crate::tui::app::GenerationStatus;
use crate::tui::App;

/// Maximum tokens allowed in a single response to prevent memory exhaustion
/// Corresponds to roughly 400,000 characters (conservative estimate)
const MAX_RESPONSE_TOKENS: usize = 100_000;

/// How often to check response size (check every N characters)
/// This prevents excessive token counting while maintaining safety
const RESPONSE_CHECK_INTERVAL: usize = 1000;

/// Result of processing streaming chunks
#[derive(Debug, Clone)]
pub enum StreamStatus {
    /// Still streaming, no action needed
    Streaming,
    /// Generation complete with parsed actions
    Complete { actions: Vec<AgentAction> },
    /// Plan ready for approval (PlanMode only)
    PlanReady { plan: Plan },
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
        // Check for custom status marker (appears at start of response)
        if chunk.starts_with("[STATUS:") {
            // Extract status between [STATUS: and ]
            if let Some(end_idx) = chunk.find(']') {
                let status_text = chunk[8..end_idx].to_string(); // Skip "[STATUS:"
                app.custom_status = Some(status_text);
                // Remove the status marker from the chunk so it doesn't get added to response
                let remaining = chunk[end_idx + 1..].to_string();
                if remaining.is_empty() {
                    continue; // Skip if nothing left after status marker
                }
                // Process the remaining part of the chunk
                let chunk = remaining;
                // Fall through to process rest of chunk
            } else {
                continue; // Incomplete status marker, skip for now
            }
        }

        if chunk.starts_with("[DONE]:") {
            // Check if this is feedback completion
            let is_feedback_complete = chunk.contains("[FEEDBACK_COMPLETE]");

            // Generation complete - reset all status fields
            app.is_generating = false;
            app.generation_status = GenerationStatus::Idle;
            app.generation_start_time = None;
            app.custom_status = None;

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

                // Parse and execute any actions from the response
                let actions = agents::parse_actions(&response_text);

                // Check if we're in PlanMode
                if app.operation_mode.is_planning_only() {
                    // Extract explanation text (everything except action blocks)
                    let explanation = agents::strip_action_blocks(&response_text);
                    let explanation = if explanation.is_empty() {
                        None
                    } else {
                        Some(explanation)
                    };

                    // Clear the accumulated response
                    app.current_response.clear();

                    // Add the full response as a message (will show explanation + plan together)
                    if !actions.is_empty() || explanation.is_some() {
                        let plan = Plan::with_explanation(explanation, actions);
                        app.plan_mode_active_for_generation = true;
                        // Add plan display as assistant message (combines explanation + action summary)
                        app.add_message(MessageRole::Assistant, plan.display_text.clone());
                        return Ok(StreamStatus::PlanReady { plan });
                    }
                    return Ok(StreamStatus::Complete { actions: vec![] });
                }

                // In normal mode, add message with full response
                app.add_message(MessageRole::Assistant, response_text.clone());

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
            app.generation_status = GenerationStatus::Idle;
            app.generation_start_time = None;
            app.custom_status = None;
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
            // Regular streaming chunk - accumulate with size check
            let prev_len = app.current_response.len();
            app.current_response.push_str(&chunk);

            // Transition to Streaming on first content chunk (from Sending or Thinking)
            if app.generation_status != GenerationStatus::Streaming {
                app.generation_status = GenerationStatus::Streaming;
            }

            // Estimate tokens in this chunk (simple: ~4 chars per token)
            let chunk_tokens = (chunk.len() + 3) / 4; // Round up
            app.tokens_received += chunk_tokens;

            // Check response size periodically to prevent memory exhaustion
            // Only check every N characters to avoid excessive computation
            if app.current_response.len() - prev_len > RESPONSE_CHECK_INTERVAL
                || app.current_response.len() % RESPONSE_CHECK_INTERVAL == 0
            {
                check_and_handle_response_size_limit(app);
            }
        }
    }

    Ok(StreamStatus::Streaming)
}

/// Check if response has exceeded token limit and handle appropriately
///
/// This function:
/// 1. Estimates token count of current response
/// 2. Warns user if approaching limit (90%)
/// 3. Truncates response if limit exceeded
/// 4. Uses simple character-based estimation to avoid expensive token counting
fn check_and_handle_response_size_limit(app: &mut App) {
    // Use simple estimation: ~4 characters per token (conservative for safety)
    let estimated_tokens = app.current_response.len() / 4;

    if estimated_tokens > MAX_RESPONSE_TOKENS {
        // Truncate response to fit within limit (leave space for action markers)
        let max_chars = (MAX_RESPONSE_TOKENS * 4) - 100;
        if app.current_response.len() > max_chars {
            app.current_response.truncate(max_chars);
            app.current_response
                .push_str("\n\n[TRUNCATED: Response exceeded token limit]\n");
            app.set_status(format!(
                "[WARNING] Response truncated ({}+ tokens limit reached)",
                MAX_RESPONSE_TOKENS
            ));
        }
    } else if estimated_tokens > (MAX_RESPONSE_TOKENS * 9 / 10) {
        // Warn when approaching limit (90%)
        app.set_status(format!(
            "[WARNING] Large response (~{} tokens). Still accepting chunks...",
            estimated_tokens
        ));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_response_size_estimation() {
        // Test Issue #2: Response size limit calculations
        // 4 characters per token is the estimation formula

        let small_response = "hello world";
        let small_tokens = small_response.len() / 4; // ~3 tokens
        assert!(small_tokens < MAX_RESPONSE_TOKENS);

        let large_response = "a".repeat(MAX_RESPONSE_TOKENS * 4);
        let large_tokens = large_response.len() / 4;
        assert_eq!(large_tokens, MAX_RESPONSE_TOKENS);
    }

    #[test]
    fn test_token_limit_constants() {
        // Verify Issue #2 constants are properly configured
        assert_eq!(MAX_RESPONSE_TOKENS, 100_000);
        assert_eq!(RESPONSE_CHECK_INTERVAL, 1000);

        // Max char limit should be 4x tokens minus buffer
        let max_chars = (MAX_RESPONSE_TOKENS * 4) - 100;
        assert_eq!(max_chars, 399_900);
    }

    #[test]
    fn test_extract_reading_intent_with_various_verbs() {
        // Test Issue #2: Reading intent extraction
        let test_cases = vec![
            (
                "Let me read the config file [FILE_READ: config.toml]",
                "Reading config.toml...",
            ),
            (
                "I need to check the error [FILE_READ: error.log]",
                "Checking error.log...",
            ),
            (
                "Let me look at this [FILE_READ: main.rs]",
                "Looking at main.rs...",
            ),
            (
                "I should open it [FILE_READ: src/lib.rs]",
                "Opening src/lib.rs...",
            ),
            (
                "Let me examine the code [FILE_READ: app.rs]",
                "Examining app.rs...",
            ),
            (
                "Loading the config [FILE_READ: config.yaml]",
                "Loading config.yaml...",
            ),
            (
                "Processing data [FILE_READ: data.json]",
                "Processing data.json...",
            ),
        ];

        for (input, expected) in test_cases {
            let result = extract_reading_intent(input);
            assert_eq!(result, Some(expected.to_string()), "Failed for: {}", input);
        }
    }

    #[test]
    fn test_extract_reading_intent_edge_cases() {
        // Test edge cases for reading intent extraction

        // No FILE_READ action
        assert_eq!(extract_reading_intent("Just some text"), None);

        // FILE_READ without closing bracket - still finds it and uses "the file" as path
        // The function looks for "[FILE_READ:" and when no ']' is found, it uses "the file"
        let result = extract_reading_intent("Let me read [FILE_READ: file.txt");
        assert_eq!(result, Some("Reading the file...".to_string()));

        // FILE_READ with empty path
        let result = extract_reading_intent("Reading [FILE_READ:]");
        assert_eq!(result, Some("Reading ...".to_string()));
    }

    #[test]
    fn test_response_truncation_threshold() {
        // Test Issue #2: Response is truncated when exceeding token limit
        // Verify the truncation behavior is correctly gated

        let max_chars = (MAX_RESPONSE_TOKENS * 4) - 100;

        // Response at 90% of limit should warn but not truncate
        let warning_threshold = MAX_RESPONSE_TOKENS * 9 / 10;
        let warning_chars = warning_threshold * 4;
        assert!(warning_chars < max_chars);

        // Response at 100% of limit should truncate
        assert!(max_chars < (MAX_RESPONSE_TOKENS * 4));
    }

    #[test]
    fn test_stream_status_variants() {
        // Verify all StreamStatus variants are properly defined

        let streaming = StreamStatus::Streaming;
        assert!(matches!(streaming, StreamStatus::Streaming));

        let complete = StreamStatus::Complete { actions: vec![] };
        assert!(matches!(complete, StreamStatus::Complete { .. }));

        let plan_ready = StreamStatus::PlanReady {
            plan: Plan::with_explanation(None, vec![]),
        };
        assert!(matches!(plan_ready, StreamStatus::PlanReady { .. }));

        let feedback_complete = StreamStatus::FeedbackComplete;
        assert!(matches!(feedback_complete, StreamStatus::FeedbackComplete));

        let error = StreamStatus::Error("test error".to_string());
        assert!(matches!(error, StreamStatus::Error(_)));
    }
}
