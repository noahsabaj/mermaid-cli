//! Subagent execution engine
//!
//! Spawns autonomous sub-agents that run in parallel, each with their own
//! conversation context and full tool access (minus `agent`).
//! Progress is tracked via shared state for live UI rendering.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::agents::{ActionResult as AgentActionResult, AgentAction};
use crate::constants::MAX_CONCURRENT_AGENTS;
use crate::models::{ChatMessage, Model, ModelConfig, StreamCallback};
use crate::prompts;
use crate::runtime::agent_loop::{self, AgentObserver, LoopControl, MAX_AGENT_ITERATIONS};
use crate::utils::MutexExt;

/// Progress state for a single running subagent.
/// Written by the subagent task, read by the UI render loop.
#[derive(Debug, Clone)]
pub struct SubagentProgress {
    pub id: usize,
    pub description: String,
    pub status: SubagentStatus,
    pub tool_uses: usize,
    pub tokens: usize,
    pub started_at: Instant,
}

/// Status of a running subagent
#[derive(Debug, Clone)]
pub enum SubagentStatus {
    Running,
    Completed,
    Failed(String),
}

/// Final result returned when a subagent finishes
#[derive(Debug, Clone)]
pub struct SubagentResult {
    pub id: usize,
    pub description: String,
    pub response: String,
    pub tool_uses: usize,
    pub tokens: usize,
    pub duration_secs: f64,
    pub success: bool,
}

/// Observer that bridges the shared agent loop to subagent progress state.
struct SubagentObserver {
    progress: Arc<Mutex<Vec<SubagentProgress>>>,
    index: usize,
}

impl AgentObserver for SubagentObserver {
    fn check_interrupt(&mut self) -> LoopControl {
        // Subagents don't get interrupted individually.
        // Parent abort (via JoinHandle::abort()) is the cancellation mechanism.
        LoopControl::Continue
    }

    fn on_status(&mut self, _message: &str) {}

    fn on_tool_result(
        &mut self,
        _tool_name: &str,
        _tool_call_id: &str,
        _action: &AgentAction,
        _result: &AgentActionResult,
    ) {
        let mut progress = self.progress.lock_mut_safe();
        if let Some(entry) = progress.get_mut(self.index) {
            entry.tool_uses += 1;
        }
    }

    fn on_error(&mut self, error: &str) {
        warn!(subagent_index = self.index, "Subagent error: {}", error);
    }

    fn on_generation_start(&mut self) {}

    fn on_generation_complete(&mut self, tokens: usize) {
        let mut progress = self.progress.lock_mut_safe();
        if let Some(entry) = progress.get_mut(self.index) {
            entry.tokens += tokens;
        }
    }
}

/// Run a single subagent to completion.
async fn run_subagent(
    model: Arc<RwLock<Box<dyn Model>>>,
    config: ModelConfig,
    id: usize,
    prompt: String,
    description: String,
    progress: Arc<Mutex<Vec<SubagentProgress>>>,
    progress_index: usize,
) -> SubagentResult {
    let started_at = Instant::now();

    // Build fresh message history for this subagent
    let system_prompt = config
        .system_prompt
        .clone()
        .unwrap_or_else(prompts::get_system_prompt);
    let mut messages = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user(prompt),
    ];

    // First model call
    let response_text = Arc::new(std::sync::Mutex::new(String::new()));
    let response_clone = Arc::clone(&response_text);
    let callback: StreamCallback = Arc::new(move |chunk: &str| {
        let mut resp = response_clone.lock_mut_safe();
        resp.push_str(chunk);
    });

    let initial_result = {
        let model_guard = model.read().await;
        model_guard.chat(&messages, &config, Some(callback)).await
    };

    let (content, initial_tool_calls, initial_tokens) = match initial_result {
        Ok(response) => {
            let callback_content = response_text.lock_mut_safe().clone();
            let content = if !callback_content.is_empty() {
                callback_content
            } else {
                response.content.clone()
            };
            let tokens = response.usage.map(|u| u.total_tokens).unwrap_or(0);
            let tool_calls = response.tool_calls.unwrap_or_default();

            {
                let mut prog = progress.lock_mut_safe();
                if let Some(entry) = prog.get_mut(progress_index) {
                    entry.tokens += tokens;
                }
            }

            (content, tool_calls, tokens)
        },
        Err(e) => {
            let error_msg = e.to_string();
            {
                let mut prog = progress.lock_mut_safe();
                if let Some(entry) = prog.get_mut(progress_index) {
                    entry.status = SubagentStatus::Failed(error_msg.clone());
                }
            }
            return SubagentResult {
                id,
                description,
                response: error_msg,
                tool_uses: 0,
                tokens: 0,
                duration_secs: started_at.elapsed().as_secs_f64(),
                success: false,
            };
        },
    };

    // If no tool calls, subagent is done after the first response
    if initial_tool_calls.is_empty() {
        {
            let mut prog = progress.lock_mut_safe();
            if let Some(entry) = prog.get_mut(progress_index) {
                entry.status = SubagentStatus::Completed;
            }
        }
        return SubagentResult {
            id,
            description,
            response: content,
            tool_uses: 0,
            tokens: initial_tokens,
            duration_secs: started_at.elapsed().as_secs_f64(),
            success: true,
        };
    }

    // Has tool calls -- enter agent loop
    let assistant_msg =
        ChatMessage::assistant(content.clone()).with_tool_calls(initial_tool_calls.clone());
    messages.push(assistant_msg);

    let mut observer = SubagentObserver {
        progress: Arc::clone(&progress),
        index: progress_index,
    };

    let loop_result = agent_loop::run_agent_loop(
        Arc::clone(&model),
        &config,
        &mut messages,
        initial_tool_calls,
        &mut observer,
        MAX_AGENT_ITERATIONS,
    )
    .await;

    match loop_result {
        Ok(result) => {
            let total_tokens = initial_tokens + result.total_tokens;
            let total_tool_uses = result.tool_results.len();
            let final_response = if result.final_response.is_empty() {
                content
            } else {
                result.final_response
            };

            {
                let mut prog = progress.lock_mut_safe();
                if let Some(entry) = prog.get_mut(progress_index) {
                    entry.status = SubagentStatus::Completed;
                    entry.tokens = total_tokens;
                    entry.tool_uses = total_tool_uses;
                }
            }

            SubagentResult {
                id,
                description,
                response: final_response,
                tool_uses: total_tool_uses,
                tokens: total_tokens,
                duration_secs: started_at.elapsed().as_secs_f64(),
                success: !result.interrupted,
            }
        },
        Err(e) => {
            let error_msg = e.to_string();
            let (tool_uses, tokens) = {
                let prog = progress.lock_mut_safe();
                prog.get(progress_index)
                    .map(|p| (p.tool_uses, p.tokens))
                    .unwrap_or((0, initial_tokens))
            };

            {
                let mut prog = progress.lock_mut_safe();
                if let Some(entry) = prog.get_mut(progress_index) {
                    entry.status = SubagentStatus::Failed(error_msg.clone());
                }
            }

            SubagentResult {
                id,
                description,
                response: error_msg,
                tool_uses,
                tokens,
                duration_secs: started_at.elapsed().as_secs_f64(),
                success: false,
            }
        },
    }
}

/// Spawn multiple subagents in parallel.
///
/// Returns JoinHandles so the caller can decide how to wait:
/// - TUI: polls `is_finished()` with `render_and_check_interrupt` between checks
/// - Non-interactive: `join_all` directly
///
/// Agents beyond `MAX_CONCURRENT_AGENTS` are returned as immediate failed results.
pub fn spawn_subagents(
    agents: Vec<(String, String)>,
    model: Arc<RwLock<Box<dyn Model>>>,
    config: &ModelConfig,
    progress: Arc<Mutex<Vec<SubagentProgress>>>,
) -> (Vec<JoinHandle<SubagentResult>>, Vec<SubagentResult>) {
    let mut handles = Vec::new();
    let mut overflow_results = Vec::new();

    // Initialize progress entries
    {
        let mut prog = progress.lock_mut_safe();
        for (i, (_prompt, description)) in agents.iter().enumerate() {
            if i < MAX_CONCURRENT_AGENTS {
                prog.push(SubagentProgress {
                    id: i,
                    description: description.clone(),
                    status: SubagentStatus::Running,
                    tool_uses: 0,
                    tokens: 0,
                    started_at: Instant::now(),
                });
            }
        }
    }

    for (i, (prompt, description)) in agents.into_iter().enumerate() {
        if i >= MAX_CONCURRENT_AGENTS {
            warn!(
                "Exceeded MAX_CONCURRENT_AGENTS ({}), skipping agent: {}",
                MAX_CONCURRENT_AGENTS, description
            );
            overflow_results.push(SubagentResult {
                id: i,
                description,
                response: format!(
                    "Exceeded maximum of {} concurrent agents. This agent was not spawned.",
                    MAX_CONCURRENT_AGENTS
                ),
                tool_uses: 0,
                tokens: 0,
                duration_secs: 0.0,
                success: false,
            });
            continue;
        }

        // Build subagent-specific config
        let mut subagent_config = config.clone();
        subagent_config.is_subagent = true;
        subagent_config.thinking_enabled = Some(false);

        let model_clone = Arc::clone(&model);
        let progress_clone = Arc::clone(&progress);

        info!(agent_id = i, description = %description, "Spawning subagent");

        let handle = tokio::spawn(async move {
            run_subagent(
                model_clone,
                subagent_config,
                i,
                prompt,
                description,
                progress_clone,
                i,
            )
            .await
        });

        handles.push(handle);
    }

    (handles, overflow_results)
}

/// Collect results from completed subagent handles.
/// Handles panicked/cancelled tasks gracefully.
pub async fn collect_subagent_results(
    handles: Vec<JoinHandle<SubagentResult>>,
    mut overflow_results: Vec<SubagentResult>,
) -> Vec<SubagentResult> {
    let mut results = Vec::with_capacity(handles.len() + overflow_results.len());

    for (i, handle) in handles.into_iter().enumerate() {
        match handle.await {
            Ok(result) => results.push(result),
            Err(e) => {
                warn!("Subagent task failed: {}", e);
                results.push(SubagentResult {
                    id: i,
                    description: "Unknown".to_string(),
                    response: format!("Agent task failed: {}", e),
                    tool_uses: 0,
                    tokens: 0,
                    duration_secs: 0.0,
                    success: false,
                });
            },
        }
    }

    results.append(&mut overflow_results);
    results.sort_by_key(|r| r.id);
    results
}

/// Format a SubagentResult into a tool result message string for the parent model.
pub fn format_subagent_tool_result(result: &SubagentResult) -> String {
    if result.success {
        format!(
            "Agent '{}' completed successfully ({} tool uses, {} tokens, {:.1}s):\n\n{}",
            result.description, result.tool_uses, result.tokens, result.duration_secs,
            result.response
        )
    } else {
        format!(
            "Agent '{}' failed: {} ({} tool uses, {} tokens, {:.1}s)",
            result.description, result.response, result.tool_uses, result.tokens,
            result.duration_secs
        )
    }
}
