//! The `enter_plan_mode` tool — the model's way to propose planning first.
//!
//! The tool body is a no-op: entering plan mode is a pure domain transition
//! (allocate the plan path, flip `session.plan`), so `handle_tool_finished`
//! performs it when it sees this tool succeed. Entering is deliberately
//! confirmation-free — the read-only floor makes it safe, and the user can
//! Shift+Tab straight back out.
//!
//! Registered `is_internal`: `build_chat_request` advertises the definition
//! only while NOT planning (and never to subagents).

use std::time::Instant;

use async_trait::async_trait;

use mermaid_domain::{ToolDefinition, ToolOutcome};

use super::super::ctx::ExecContext;
use super::ToolExecutor;

pub struct EnterPlanModeTool;

#[async_trait]
impl ToolExecutor for EnterPlanModeTool {
    fn name(&self) -> &'static str {
        mermaid_domain::plan::ENTER_PLAN_MODE_TOOL
    }

    fn schema(&self) -> ToolDefinition {
        mermaid_domain::plan::enter_plan_mode_definition()
    }

    fn is_internal(&self) -> bool {
        true
    }

    async fn execute(&self, _args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let start = Instant::now();
        if ctx.plan_file.is_some() {
            return ToolOutcome::error("already in plan mode", start.elapsed().as_secs_f64());
        }
        ToolOutcome::success(
            "Plan mode is on: the read-only floor now applies and the next system prompt names \
             the plan file to author. Ground the design in the code, ask the user about genuine \
             preferences, and finish by calling exit_plan_mode.",
            "plan mode on",
            start.elapsed().as_secs_f64(),
        )
    }
}
