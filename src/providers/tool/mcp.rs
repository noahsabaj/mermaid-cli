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

use crate::domain::{ToolDefinition, ToolMetadata, ToolOutcome, ToolRunMetadata, ToolStatus};
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
        )
        .await
        {
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
                Ok(tool_result) => outcome_from_mcp(
                    &tool_result,
                    server_name,
                    tool_name,
                    start.elapsed().as_secs_f64(),
                ),
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

/// Map a completed MCP `tools/call` result onto a `ToolOutcome`, honoring the
/// MCP `isError` flag (#91). A successful JSON-RPC round-trip can still carry a
/// tool-level failure (`isError: true`); in that case the model must see an
/// *error* outcome — the server's own content verbatim, status `Error` — not a
/// success. Pure so both branches are unit-tested.
fn outcome_from_mcp(
    tool_result: &crate::mcp::McpToolResult,
    server_name: &str,
    tool_name: &str,
    duration_secs: f64,
) -> ToolOutcome {
    let (text, images) = McpServerManager::format_tool_result(tool_result);
    let verb = if tool_result.is_error {
        "failed"
    } else {
        "completed"
    };
    // Build the success-shaped outcome first so `model_content == text` and the
    // metadata/images wiring matches the non-error path exactly (with_metadata
    // must precede with_images — with_metadata replaces the metadata box).
    let mut outcome = ToolOutcome::success(
        text,
        format!("{}:{} {}", server_name, tool_name, verb),
        duration_secs,
    )
    .with_metadata(mcp_metadata(server_name, tool_name));
    if let Some(images) = images {
        outcome = outcome.with_images(images);
    }
    if tool_result.is_error {
        outcome = outcome.with_status(ToolStatus::Error);
    }
    outcome
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

    #[test]
    fn outcome_from_mcp_is_error_maps_to_error_status_preserving_content() {
        use crate::mcp::{ContentBlock, McpToolResult};
        let result = McpToolResult {
            content: vec![ContentBlock::Text("boom: rate limited".into())],
            is_error: true,
        };
        let outcome = outcome_from_mcp(&result, "slack", "send", 0.5);
        assert_eq!(outcome.status, ToolStatus::Error);
        // The model sees the server's own error content verbatim — no "Error:" prefix.
        assert_eq!(outcome.output(), "boom: rate limited");
        // `error` is populated so the renderer shows the failure, not "[cancelled]".
        assert_eq!(outcome.error_message(), Some("boom: rate limited"));
        assert_eq!(outcome.summary, "slack:send failed");
    }

    #[test]
    fn outcome_from_mcp_success_maps_to_success_status() {
        use crate::mcp::{ContentBlock, McpToolResult};
        let result = McpToolResult {
            content: vec![ContentBlock::Text("ok".into())],
            is_error: false,
        };
        let outcome = outcome_from_mcp(&result, "slack", "send", 0.5);
        assert_eq!(outcome.status, ToolStatus::Success);
        assert_eq!(outcome.output(), "ok");
        assert_eq!(outcome.error_message(), None);
        assert_eq!(outcome.summary, "slack:send completed");
    }
}
