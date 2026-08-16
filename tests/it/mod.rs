//! Integration test submodules.

mod agent_loop_stubbed;
mod auto_classifier_stubbed;
mod compaction_stubbed;
#[cfg(unix)]
mod daemon_integration;
mod effect_cancel;
mod engine_handle_stubbed;
mod feedback_cli;
mod lint_policy_drift;
mod memory_consolidation_stubbed;
mod prompt_tool_drift;
#[cfg(unix)]
mod pty_exit;
mod pty_frame;
mod pty_visual;
mod readme_drift;
mod reducer_flows;
mod replay_determinism;
mod run_event_stream;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
mod sandbox_fs;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
mod sandbox_network;
mod storage_coordinator_cli;
mod subagent_lifecycle_stubbed;
mod subagent_worktree;
mod subagent_worktree_scripted;
