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

use crate::domain::{ToolDefinition, ToolMetadata, ToolOutcome, ToolRunMetadata};
use crate::mcp::{McpServerManager, manager_ref};

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

    fn is_internal(&self) -> bool {
        true
    }

    fn schema(&self) -> ToolDefinition {
        // The proxy isn't itself advertised to the model; per-MCP
        // tool schemas are sourced from `State.mcp.servers.*.tools`
        // in the reducer. This schema is kept for registry-cohesion
        // but never lands in `request.tools`.
        ToolDefinition {
            name: "mcp_proxy".to_string(),
            description: "Internal dispatch target for mcp__* tool calls.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "server_name": { "type": "string" },
                    "tool_name": { "type": "string" },
                    "arguments": { "type": "object" }
                },
                "required": ["server_name", "tool_name"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        // Args shape: { server_name, tool_name, arguments }. The effect
        // runner constructs this from the model-emitted tool call.
        let Some(server_name) = args.get("server_name").and_then(|v| v.as_str()) else {
            return ToolOutcome::error("mcp_proxy requires 'server_name'", 0.0);
        };
        let Some(tool_name) = args.get("tool_name").and_then(|v| v.as_str()) else {
            return ToolOutcome::error("mcp_proxy requires 'tool_name'", 0.0);
        };
        let tool_args = args
            .get("arguments")
            .cloned()
            .unwrap_or(serde_json::json!({}));

        // Safety gate: MCP servers are untrusted external processes.
        // ReadOnly (or a Deny override) blocks them; other modes proceed.
        if let Some(blocked) = super::policy_gate::gate_external(
            &ctx,
            "mcp_proxy",
            crate::runtime::ToolCategory::Mcp,
            format!("mcp {}__{}", server_name, tool_name),
            &args,
        ) {
            return blocked;
        }

        // If init is still racing, park briefly — the model might
        // have sprinted ahead on its very first message. If it never
        // finishes within a reasonable bound we still fall through
        // to a clean error rather than hanging forever.
        if !manager_ref::is_ready() {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                manager_ref::wait_ready(),
            )
            .await;
        }
        let Some(manager) = manager_ref::get() else {
            return ToolOutcome::error("MCP servers not initialized", 0.0);
        };

        let start = std::time::Instant::now();
        let call = manager.call_tool(server_name, tool_name, &tool_args);

        tokio::select! {
            biased;
            _ = ctx.token.cancelled() => ToolOutcome::cancelled(),
            result = call => match result {
                Ok(tool_result) => {
                    let (text, images) = McpServerManager::format_tool_result(&tool_result);
                    let mut outcome = ToolOutcome::success(
                        text,
                        format!("{}:{} completed", server_name, tool_name),
                        start.elapsed().as_secs_f64(),
                    )
                    .with_metadata(mcp_metadata(server_name, tool_name));
                    if let Some(images) = images {
                        outcome = outcome.with_images(images);
                    }
                    outcome
                },
                Err(e) => ToolOutcome::error(
                    format!("mcp_proxy({}:{}): {}", server_name, tool_name, e),
                    start.elapsed().as_secs_f64(),
                )
                .with_metadata(mcp_metadata(server_name, tool_name)),
            },
        }
    }
}

fn mcp_metadata(server_name: &str, tool_name: &str) -> ToolRunMetadata {
    ToolRunMetadata {
        detail: ToolMetadata::Mcp {
            server: server_name.to_string(),
            tool: tool_name.to_string(),
        },
        ..ToolRunMetadata::default()
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
        assert_eq!(outcome.status, crate::domain::ToolStatus::Error);
    }

    #[tokio::test]
    async fn missing_tool_name_errors() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let outcome = McpToolProxy
            .execute(serde_json::json!({"server_name": "x"}), ctx)
            .await;
        assert_eq!(outcome.status, crate::domain::ToolStatus::Error);
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
        assert_eq!(outcome.status, crate::domain::ToolStatus::Error);
    }
}
