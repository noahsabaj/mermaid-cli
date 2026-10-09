//! `list_mcp_resources` and `read_mcp_resource`: MCP resources as two
//! built-in tools.
//!
//! The reducer advertises them only while a ready server declared the
//! `resources` capability (`mermaid_domain::mcp_resources`), so both are
//! registered `internal` here — the registry dispatches them but never lists
//! them itself. Each call goes through the same policy gate as an MCP tool
//! call (`gate_external_mcp`, which also makes them non-allowlistable): MCP
//! servers are untrusted external processes, so `read_only` blocks resource
//! access exactly as it blocks MCP tools. Both protocol methods are
//! read-only BY DEFINITION, so they gate as read-hinted calls — the
//! external-writes floor, which exists for write-shaped tools, does not
//! apply.

use async_trait::async_trait;

use crate::mcp::{McpResource, McpServerManager, ResourceContents, manager_ref};
use mermaid_domain::mcp_resources::{
    LIST_MCP_RESOURCES, READ_MCP_RESOURCE, list_definition, read_definition,
};
use mermaid_domain::{ToolDefinition, ToolMetadata, ToolOutcome, ToolRunMetadata};

use super::super::ctx::ExecContext;
use super::ToolExecutor;

/// Model-visible output cap, in characters (head and tail kept). A resource
/// is whatever the server serves — a whole table or log — so it is bounded
/// like a file read rather than passed through like a tool's reply.
const MAX_RESOURCE_OUTPUT_CHARS: usize = 100_000;

pub struct ListMcpResourcesTool;

pub struct ReadMcpResourceTool;

#[async_trait]
impl ToolExecutor for ListMcpResourcesTool {
    fn name(&self) -> &'static str {
        LIST_MCP_RESOURCES
    }

    fn is_internal(&self) -> bool {
        true
    }

    fn schema(&self) -> ToolDefinition {
        list_definition("")
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let filter = args
            .get("server")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        let Some(manager) = ready_manager().await else {
            return not_initialized();
        };
        let servers = match filter {
            Some(server) => vec![server.to_string()],
            None => manager.resource_servers(),
        };
        if servers.is_empty() {
            return ToolOutcome::error("No connected MCP server provides resources", None);
        }
        let target = filter.unwrap_or("*");
        if let Some(blocked) = gate(&ctx, target, "resources/list", serde_json::json!({})).await {
            return blocked;
        }

        let start = std::time::Instant::now();
        let listing = async {
            let mut entries = Vec::new();
            for server in &servers {
                entries.push(list_entry(manager, server).await);
            }
            entries
        };
        let entries = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
            entries = listing => entries,
        };
        let count: usize = entries
            .iter()
            .filter_map(|e| e.get("resources").and_then(|r| r.as_array()))
            .map(Vec::len)
            .sum();
        let text = serde_json::to_string_pretty(&entries).unwrap_or_default();
        ToolOutcome::success(
            mermaid_model::utils::truncate_middle(&text, MAX_RESOURCE_OUTPUT_CHARS),
            format!("{count} MCP resource(s) from {} server(s)", servers.len()),
            start.elapsed().as_secs_f64(),
        )
        .with_metadata(metadata(target, "resources/list"))
    }
}

#[async_trait]
impl ToolExecutor for ReadMcpResourceTool {
    fn name(&self) -> &'static str {
        READ_MCP_RESOURCE
    }

    fn is_internal(&self) -> bool {
        true
    }

    fn schema(&self) -> ToolDefinition {
        read_definition("")
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let str_arg = |key: &str| {
            args.get(key)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        };
        let Some(server) = str_arg("server") else {
            return ToolOutcome::error("read_mcp_resource requires 'server'", None);
        };
        let Some(uri) = str_arg("uri") else {
            return ToolOutcome::error("read_mcp_resource requires 'uri'", None);
        };
        let Some(manager) = ready_manager().await else {
            return not_initialized();
        };
        if let Some(blocked) = gate(
            &ctx,
            server,
            "resources/read",
            serde_json::json!({ "uri": uri }),
        )
        .await
        {
            return blocked;
        }

        let start = std::time::Instant::now();
        tokio::select! {
            biased;
            _ = ctx.token.cancelled() => ToolOutcome::cancelled(),
            result = manager.read_resource(server, uri) => match result {
                Ok(contents) => {
                    let (text, images) = format_contents(&contents);
                    let mut outcome = ToolOutcome::success(
                        mermaid_model::utils::truncate_middle(&text, MAX_RESOURCE_OUTPUT_CHARS),
                        format!("{server}: read {uri}"),
                        start.elapsed().as_secs_f64(),
                    )
                    .with_metadata(metadata(server, "resources/read"));
                    if !images.is_empty() {
                        outcome = outcome.with_images(images);
                    }
                    outcome
                },
                Err(e) => ToolOutcome::error(
                    format!("read_mcp_resource({server}, {uri}): {e}"),
                    Some(start.elapsed().as_secs_f64()),
                )
                .with_metadata(metadata(server, "resources/read")),
            },
        }
    }
}

/// The installed manager once startup has settled — waiting, bounded, for a
/// model that calls on its very first message (same race as
/// `McpToolProxy`). The wait runs BEFORE the gate; it has no side effects.
async fn ready_manager() -> Option<&'static McpServerManager> {
    if !manager_ref::is_ready() {
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            manager_ref::wait_ready(),
        )
        .await;
    }
    manager_ref::get().map(AsRef::as_ref)
}

fn not_initialized() -> ToolOutcome {
    ToolOutcome::error("MCP servers not initialized", None)
}

/// The MCP policy gate, fed the same `{server_name, tool_name, arguments}`
/// shape the proxy uses, so the approval prompt and the Auto classifier see
/// `mcp <server>__resources/read({"uri":…})`.
async fn gate(
    ctx: &ExecContext,
    server: &str,
    method: &str,
    arguments: serde_json::Value,
) -> Option<ToolOutcome> {
    let args = serde_json::json!({
        "server_name": server,
        "tool_name": method,
        "arguments": arguments,
    });
    super::policy_gate::gate_external_mcp(ctx, format!("mcp {server} {method}"), &args, true).await
}

/// One server's listing as a JSON object: its resources, or the error that
/// stopped them — one failing server never hides the others'.
async fn list_entry(manager: &McpServerManager, server: &str) -> serde_json::Value {
    match manager.list_resources(server).await {
        Ok((raw, resources)) => serde_json::json!({
            "server": raw,
            "resources": resources_json(&resources),
        }),
        Err(e) => serde_json::json!({ "server": server, "error": e.to_string() }),
    }
}

fn resources_json(resources: &[McpResource]) -> serde_json::Value {
    serde_json::to_value(resources).unwrap_or_else(|_| serde_json::json!([]))
}

/// Render `resources/read` contents for the model: text items verbatim under
/// a header naming their URI, images attached through the multimodal channel
/// MCP tool images already use, and any other binary content summarized by
/// type and decoded size instead of dumping base64 into the context.
fn format_contents(contents: &[ResourceContents]) -> (String, Vec<String>) {
    let mut parts = Vec::with_capacity(contents.len());
    let mut images = Vec::new();
    for item in contents {
        let mime = item.mime_type.as_deref().unwrap_or("unknown type");
        match (&item.text, &item.blob) {
            (Some(text), _) => parts.push(format!("[{} ({mime})]\n{text}", item.uri)),
            (None, Some(blob)) => {
                let bytes = base64_decoded_len(blob);
                if mime.starts_with("image/") {
                    images.push(blob.clone());
                    parts.push(format!(
                        "[{} ({mime}): image attached, {bytes} bytes]",
                        item.uri
                    ));
                } else {
                    parts.push(format!(
                        "[{} ({mime}): binary content, {bytes} bytes, not shown]",
                        item.uri
                    ));
                }
            },
            (None, None) => parts.push(format!("[{} ({mime}): empty]", item.uri)),
        }
    }
    if parts.is_empty() {
        parts.push("The resource returned no contents.".to_string());
    }
    (parts.join("\n\n"), images)
}

/// Decoded byte length of a base64 string, from its length and padding.
fn base64_decoded_len(blob: &str) -> usize {
    let trimmed = blob.trim_end();
    let padding = trimmed.bytes().rev().take_while(|b| *b == b'=').count();
    (trimmed.len() / 4 * 3).saturating_sub(padding)
}

fn metadata(server: &str, method: &str) -> ToolRunMetadata {
    ToolRunMetadata {
        detail: ToolMetadata::Mcp {
            server: server.to_string(),
            tool: method.to_string(),
        },
        ..ToolRunMetadata::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ctx::test_exec_context;
    use mermaid_domain::{ToolCallId, ToolStatus, TurnId};
    use std::path::PathBuf;

    fn contents(mime: &str, text: Option<&str>, blob: Option<&str>) -> ResourceContents {
        ResourceContents {
            uri: "file:///r".to_string(),
            mime_type: Some(mime.to_string()),
            text: text.map(String::from),
            blob: blob.map(String::from),
        }
    }

    #[test]
    fn text_contents_render_verbatim_under_their_uri() {
        let (text, images) = format_contents(&[contents("text/plain", Some("hello"), None)]);
        assert_eq!(text, "[file:///r (text/plain)]\nhello");
        assert!(images.is_empty());
    }

    #[test]
    fn binary_contents_are_summarized_not_dumped() {
        // "aGVsbG8=" is base64 for the 5 bytes "hello".
        let (text, images) =
            format_contents(&[contents("application/pdf", None, Some("aGVsbG8="))]);
        assert_eq!(
            text,
            "[file:///r (application/pdf): binary content, 5 bytes, not shown]"
        );
        assert!(
            !text.contains("aGVsbG8"),
            "no base64 in the model's context"
        );
        assert!(images.is_empty());
    }

    #[test]
    fn image_blobs_ride_the_image_channel() {
        let (text, images) = format_contents(&[contents("image/png", None, Some("iVBORw=="))]);
        assert_eq!(images, ["iVBORw=="]);
        assert!(text.contains("image attached, 4 bytes"), "{text}");
    }

    #[test]
    fn base64_length_accounts_for_padding() {
        assert_eq!(base64_decoded_len(""), 0);
        assert_eq!(base64_decoded_len("aGVsbG8="), 5);
        assert_eq!(base64_decoded_len("aGVsbA=="), 4);
        assert_eq!(base64_decoded_len("aGVsbG9v"), 6);
    }

    #[tokio::test]
    async fn read_requires_server_and_uri() {
        for args in [
            serde_json::json!({"uri": "file:///r"}),
            serde_json::json!({"server": "s"}),
        ] {
            let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
            let outcome = ReadMcpResourceTool.execute(args, ctx).await;
            assert_eq!(outcome.status, ToolStatus::Error);
        }
    }

    #[test]
    fn schemas_name_their_executors() {
        assert_eq!(
            ListMcpResourcesTool.schema().name,
            ListMcpResourcesTool.name()
        );
        assert_eq!(
            ReadMcpResourceTool.schema().name,
            ReadMcpResourceTool.name()
        );
    }
}
