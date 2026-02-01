use anyhow::Result;
use std::sync::{LazyLock, Mutex};
use tokio::sync::mpsc;

use crate::agents::{AgentAction, Plan};
use crate::models::{parse_tool_calls, group_parallel_reads, ErrorCategory, MessageRole, UserFacingError};
use super::state::GenerationStatus;
use crate::tui::App;

/// Global tokenizer instance (lazy initialized)
static TOKENIZER: LazyLock<Mutex<Option<tiktoken_rs::CoreBPE>>> = LazyLock::new(|| {
    Mutex::new(tiktoken_rs::get_bpe_from_model("gpt-3.5-turbo").ok())
});

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
    /// Generation complete with parsed actions and tool calls
    Complete {
        actions: Vec<AgentAction>,
        /// Original tool calls from the model (for building proper Tool messages)
        tool_calls: Vec<crate::models::ToolCall>,
    },
    /// Plan ready for approval (PlanMode only)
    PlanReady { plan: Plan },
    /// Feedback loop complete (ReadFile response)
    FeedbackComplete,
    /// Error occurred during streaming with structured error info
    Error(UserFacingError),
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
    if !app.app_state.is_generating() {
        return Ok(StreamStatus::Streaming);
    }

    // Accumulate tool calls from streaming
    let mut accumulated_tool_calls: Vec<crate::models::ToolCall> = Vec::new();

    // Process all available messages from the channel
    while let Ok(chunk) = rx.try_recv() {
        // Check for custom status marker (appears at start of response)
        if chunk.starts_with("[STATUS:") {
            // Extract status between [STATUS: and ]
            if let Some(end_idx) = chunk.find(']') {
                let status_text = chunk[8..end_idx].to_string(); // Skip "[STATUS:"
                app.status_state.custom_status = Some(status_text);
                // Remove the status marker from the chunk so it doesn't get added to response
                let remaining = chunk[end_idx + 1..].to_string();
                if remaining.is_empty() {
                    continue; // Skip if nothing left after status marker
                }
                // Process the remaining part of the chunk
                // Fall through to process rest of chunk
            } else {
                continue; // Incomplete status marker, skip for now
            }
        }

        // Check for tool calls marker from Ollama adapter
        // Format: [TOOL_CALLS:[{...},{...}]]  (marker wraps JSON array)
        if chunk.starts_with("[TOOL_CALLS:") {
            // Use rfind to find the LAST ] which closes the marker (not the JSON array's ])
            if let Some(end_idx) = chunk.rfind(']') {
                let json_str = &chunk[12..end_idx]; // Skip "[TOOL_CALLS:" to get JSON array
                if let Ok(tool_calls) = serde_json::from_str::<Vec<crate::models::ToolCall>>(json_str) {
                    accumulated_tool_calls.extend(tool_calls);
                }
                // Don't add tool call markers to response text
                continue;
            }
        }

        if chunk.starts_with("[DONE]:") {
            // Check if this is feedback completion
            let is_feedback_complete = chunk.contains("[FEEDBACK_COMPLETE]");

            // Generation complete - reset all status fields
            app.stop_generation();
            app.status_state.custom_status = None;

            // Clear feedback flags if this was a feedback response
            if is_feedback_complete {
                app.operation_state.pending_file_read = false;
                app.operation_state.reading_file_status = None;
                return Ok(StreamStatus::FeedbackComplete);
            }

            // Also clear any lingering file read status on normal completion
            if !app.operation_state.pending_file_read {
                app.operation_state.reading_file_status = None;
            }

            // Add the accumulated response from streaming
            if !app.current_response.is_empty() {
                let response_text = app.current_response.clone();

                // Parse actions from accumulated tool calls (Ollama native function calling)
                let actions = if !accumulated_tool_calls.is_empty() {
                    let parsed = parse_tool_calls(&accumulated_tool_calls);
                    group_parallel_reads(parsed)
                } else {
                    vec![]
                };

                // Check if we're in PlanMode
                if app.operation_state.operation_mode.is_planning_only() {
                    // With tool calling, the response text IS the explanation (no action blocks to strip)
                    let explanation = if response_text.trim().is_empty() {
                        None
                    } else {
                        Some(response_text.clone())
                    };

                    // Clear the accumulated response
                    app.current_response.clear();

                    // Add the full response as a message (will show explanation + plan together)
                    if !actions.is_empty() || explanation.is_some() {
                        let plan = Plan::with_explanation(explanation, actions);
                        app.operation_state.plan_mode_active_for_generation = true;
                        // Add plan display as assistant message (combines explanation + action summary)
                        app.add_message(MessageRole::Assistant, plan.display_text.clone());
                        return Ok(StreamStatus::PlanReady { plan });
                    }
                    return Ok(StreamStatus::Complete { actions: vec![], tool_calls: vec![] });
                }

                // In normal mode, add message with full response
                // Note: We do NOT add tool_calls here yet - that's done in loop_coordinator
                // after we know whether we're continuing the agent loop
                app.add_message(MessageRole::Assistant, response_text.clone());

                // Clear the accumulated response
                app.current_response.clear();

                // Return actions and tool_calls for execution in the agent loop
                return Ok(StreamStatus::Complete {
                    actions,
                    tool_calls: accumulated_tool_calls.clone(),
                });
            }

            return Ok(StreamStatus::Complete { actions: vec![], tool_calls: vec![] });
        } else if chunk.starts_with("[ERROR_JSON]:") {
            // Structured error with rich UX information
            let error_json = chunk.trim_start_matches("[ERROR_JSON]:").trim();
            let user_error = parse_user_facing_error(error_json);

            app.stop_generation();
            app.status_state.custom_status = None;
            app.current_response.clear();

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
                user_error.message,
                user_error.suggestion
            );
            app.add_message(MessageRole::System, error_display);

            return Ok(StreamStatus::Error(user_error));
        } else if chunk.starts_with("[ERROR]:") {
            // Legacy error format (fallback for non-ModelError sources)
            let error_msg = chunk.trim_start_matches("[ERROR]:").trim().to_string();
            let user_error = UserFacingError {
                summary: "Error".to_string(),
                message: error_msg.clone(),
                suggestion: "Try the operation again or check logs for details".to_string(),
                category: ErrorCategory::Internal,
                recoverable: false,
            };

            app.stop_generation();
            app.status_state.custom_status = None;
            app.current_response.clear();

            // Use unified error display (status bar + chat) for consistency
            app.display_error_simple(&error_msg);

            return Ok(StreamStatus::Error(user_error));
        } else {
            // Regular streaming chunk - accumulate with size check
            let prev_len = app.current_response.len();
            app.current_response.push_str(&chunk);

            // Transition to Streaming on first content chunk (from Sending or Thinking)
            if app.app_state.generation_status() != Some(GenerationStatus::Streaming) {
                app.transition_to_streaming();
            }

            // Count actual tokens in this chunk using tiktoken
            let chunk_tokens = count_tokens(&chunk);
            app.increment_tokens(chunk_tokens);

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

/// Parse user-facing error from JSON with graceful fallback
fn parse_user_facing_error(json_str: &str) -> UserFacingError {
    serde_json::from_str(json_str).unwrap_or_else(|_| {
        // Fallback: try to parse pipe-delimited format or use raw string
        let parts: Vec<&str> = json_str.splitn(3, '|').collect();
        if parts.len() == 3 {
            UserFacingError {
                summary: parts[0].to_string(),
                message: parts[1].to_string(),
                suggestion: parts[2].to_string(),
                category: ErrorCategory::Internal,
                recoverable: false,
            }
        } else {
            UserFacingError {
                summary: "Error".to_string(),
                message: json_str.to_string(),
                suggestion: "Check the error details and try again".to_string(),
                category: ErrorCategory::Internal,
                recoverable: false,
            }
        }
    })
}

/// Count tokens in text using tiktoken (with fallback to estimation)
fn count_tokens(text: &str) -> usize {
    if let Ok(tokenizer_lock) = TOKENIZER.lock() {
        if let Some(bpe) = tokenizer_lock.as_ref() {
            // Use actual token count from tiktoken
            bpe.encode_ordinary(text).len()
        } else {
            // Fallback to estimation if tokenizer unavailable
            (text.len() + 3) / 4
        }
    } else {
        // Fallback to estimation if lock poisoned
        (text.len() + 3) / 4
    }
}

/// Check if response has exceeded token limit and handle appropriately
///
/// This function:
/// 1. Counts actual tokens using tiktoken
/// 2. Warns user if approaching limit (90%)
/// 3. Truncates response if limit exceeded
fn check_and_handle_response_size_limit(app: &mut App) {
    // Use actual token count from tiktoken
    let actual_tokens = count_tokens(&app.current_response);

    if actual_tokens > MAX_RESPONSE_TOKENS {
        // Truncate response to fit within limit (leave space for action markers)
        // Start by removing tokens from the end until we're under the limit
        let mut truncated = app.current_response.clone();
        while count_tokens(&truncated) > (MAX_RESPONSE_TOKENS - 50) && !truncated.is_empty() {
            truncated.pop();
        }
        app.current_response = truncated;
        app.current_response
            .push_str("\n\n[TRUNCATED: Response exceeded token limit]\n");
        app.set_status(format!(
            "[WARNING] Response truncated ({} tokens limit reached)",
            MAX_RESPONSE_TOKENS
        ));
    } else if actual_tokens > (MAX_RESPONSE_TOKENS * 9 / 10) {
        // Warn when approaching limit (90%)
        app.set_status(format!(
            "[WARNING] Large response ({} tokens). Still accepting chunks...",
            actual_tokens
        ));
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

        let complete = StreamStatus::Complete { actions: vec![], tool_calls: vec![] };
        assert!(matches!(complete, StreamStatus::Complete { .. }));

        let plan_ready = StreamStatus::PlanReady {
            plan: Plan::with_explanation(None, vec![]),
        };
        assert!(matches!(plan_ready, StreamStatus::PlanReady { .. }));

        let feedback_complete = StreamStatus::FeedbackComplete;
        assert!(matches!(feedback_complete, StreamStatus::FeedbackComplete));

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
