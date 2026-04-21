//! MCP tool dispatch.
//!
//! Every MCP server advertises tools with names like `mcp__slack__send_message`.
//! The reducer doesn't care about MCP mechanics — it just sees a tool
//! call with that prefix and dispatches here. `McpToolProxy` parses
//! the `server_name / tool_name` split and delegates to the process-
//! global `McpServerManager`.
//!
//! The manager is a `OnceLock` initialized during startup (see
//! `crate::mcp::server_manager`). The proxy doesn't own any state —
//! it's a thin function packaged as a `ToolExecutor` so the registry
//! can dispatch MCP calls uniformly with built-in tools.

use async_trait::async_trait;

use crate::agents::get_mcp_manager;
use crate::domain::ToolOutcome;
use crate::mcp::McpServerManager;

use super::super::ctx::ExecContext;
use super::ToolExecutor;

/// `mcp_proxy` isn't an actual tool name the model sees — it's the
/// dispatch target for every `mcp__*` tool call. The effect runner
/// routes MCP-prefixed calls to this impl.
pub struct McpToolProxy;

#[async_trait]
impl ToolExecutor for McpToolProxy {
    fn name(&self) -> &'static str {
        "mcp_proxy"
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        // Args shape: { server_name, tool_name, arguments }. The effect
        // runner constructs this from the model-emitted tool call.
        let Some(server_name) = args.get("server_name").and_then(|v| v.as_str()) else {
            return ToolOutcome::Error {
                error: "mcp_proxy requires 'server_name'".to_string(),
                duration_secs: 0.0,
            };
        };
        let Some(tool_name) = args.get("tool_name").and_then(|v| v.as_str()) else {
            return ToolOutcome::Error {
                error: "mcp_proxy requires 'tool_name'".to_string(),
                duration_secs: 0.0,
            };
        };
        let tool_args = args
            .get("arguments")
            .cloned()
            .unwrap_or(serde_json::json!({}));

        let Some(manager) = get_mcp_manager() else {
            return ToolOutcome::Error {
                error: "MCP servers not initialized".to_string(),
                duration_secs: 0.0,
            };
        };

        let start = std::time::Instant::now();
        let call = manager.call_tool(server_name, tool_name, &tool_args);

        tokio::select! {
            biased;
            _ = ctx.token.cancelled() => ToolOutcome::Cancelled,
            result = call => match result {
                Ok(tool_result) => {
                    let (text, images) = McpServerManager::format_tool_result(&tool_result);
                    ToolOutcome::Finished {
                        output: text,
                        images,
                        duration_secs: start.elapsed().as_secs_f64(),
                    }
                },
                Err(e) => ToolOutcome::Error {
                    error: format!("mcp_proxy({}:{}): {}", server_name, tool_name, e),
                    duration_secs: start.elapsed().as_secs_f64(),
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ToolCallId, TurnId};
    use crate::providers::ctx::test_exec_context;
    use std::path::PathBuf;

    #[tokio::test]
    async fn missing_server_name_errors() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let outcome = McpToolProxy
            .execute(serde_json::json!({"tool_name": "x"}), ctx)
            .await;
        assert!(matches!(outcome, ToolOutcome::Error { .. }));
    }

    #[tokio::test]
    async fn missing_tool_name_errors() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let outcome = McpToolProxy
            .execute(serde_json::json!({"server_name": "x"}), ctx)
            .await;
        assert!(matches!(outcome, ToolOutcome::Error { .. }));
    }

    #[tokio::test]
    async fn uninitialized_manager_errors_cleanly() {
        // If the global OnceLock hasn't been initialized in this test
        // context, calling through should error rather than panic.
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let outcome = McpToolProxy
            .execute(
                serde_json::json!({"server_name": "s", "tool_name": "t"}),
                ctx,
            )
            .await;
        // Either Error (uninitialized) or Error (server not found) —
        // both acceptable; the test asserts *not Finished*.
        assert!(matches!(outcome, ToolOutcome::Error { .. }));
    }
}
