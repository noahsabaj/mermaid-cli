//! The two MCP resource tools (pure): `list_mcp_resources` and
//! `read_mcp_resource`.
//!
//! Resources are an MCP server's read-only data surface (`resources/list`,
//! `resources/read`), separate from its tools. Rather than one tool per server,
//! the model gets two fixed built-ins that take the server as an argument —
//! advertised only while at least one READY server declared the `resources`
//! capability, so a session without one pays nothing for them. They are not
//! deferred behind `tool_search`: two small schemas are cheaper than the
//! search round-trip that would discover them. Execution is in the shell
//! (`providers::tool::mcp_resources`), through the same policy gate as MCP
//! tool calls.

use super::cmd::ToolDefinition;
use super::state::{McpServerStatus, State};

/// Lists the resources of every resources-capable server (or one).
pub const LIST_MCP_RESOURCES: &str = "list_mcp_resources";
/// Reads one resource by server and URI.
pub const READ_MCP_RESOURCE: &str = "read_mcp_resource";

/// Ready servers that advertised `resources`, sorted by name.
#[must_use]
pub fn resource_servers(state: &State) -> Vec<&str> {
    let mut servers: Vec<&str> = state
        .mcp
        .servers
        .iter()
        .filter(|(_, entry)| entry.resources && matches!(entry.status, McpServerStatus::Ready))
        .map(|(name, _)| name.as_str())
        .collect();
    servers.sort_unstable();
    servers
}

/// Both resource tool definitions while any ready server supports
/// resources, else none. The descriptions name the servers, so the model
/// knows which `server` values are valid without a listing call.
#[must_use]
pub fn resource_tool_definitions(state: &State) -> Vec<ToolDefinition> {
    let servers = resource_servers(state);
    if servers.is_empty() {
        return Vec::new();
    }
    let listing = servers.join(", ");
    vec![list_definition(&listing), read_definition(&listing)]
}

/// The description's closing sentence naming the servers; empty for the
/// registry's static copy, which never reaches a model.
fn servers_sentence(servers: &str) -> String {
    if servers.is_empty() {
        String::new()
    } else {
        format!(" Servers with resources: {servers}.")
    }
}

/// `list_mcp_resources` schema. `servers` is the comma-joined server list
/// for the description.
#[must_use]
pub fn list_definition(servers: &str) -> ToolDefinition {
    ToolDefinition {
        name: LIST_MCP_RESOURCES.to_string(),
        description: format!(
            "List resources (files, records, documents) that connected MCP servers expose, \
             as JSON with each resource's server, uri, name, description and mimeType. \
             Read one with read_mcp_resource.{}",
            servers_sentence(servers)
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": "Only list this server's resources. Omit to list every server's."
                }
            }
        }),
    }
}

/// `read_mcp_resource` schema; see [`list_definition`] for `servers`.
#[must_use]
pub fn read_definition(servers: &str) -> ToolDefinition {
    ToolDefinition {
        name: READ_MCP_RESOURCE.to_string(),
        description: format!(
            "Read one MCP resource by server and URI (as list_mcp_resources reports them). \
             Returns its text; binary content is summarized by type and size.{}",
            servers_sentence(servers)
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": "The server that owns the resource."
                },
                "uri": {
                    "type": "string",
                    "description": "The resource URI."
                }
            },
            "required": ["server", "uri"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{McpServerEntry, McpServerStatus};
    use crate::{Config, McpServerConfig};
    use chrono::TimeZone;
    use std::path::PathBuf;

    fn state_with(servers: &[(&str, McpServerStatus, bool)]) -> State {
        let now = chrono::Local.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let mut state = State::new(
            Config::default(),
            PathBuf::from("/tmp"),
            "test/model".into(),
            now,
            PathBuf::from("/tmp"),
        );
        for (name, status, resources) in servers {
            state.mcp.servers.insert(
                (*name).to_string(),
                McpServerEntry {
                    config: McpServerConfig::default(),
                    status: status.clone(),
                    tools: Vec::new(),
                    resources: *resources,
                },
            );
        }
        state
    }

    fn names(state: &State) -> Vec<String> {
        crate::tool_search::mcp_tool_definitions(state)
            .into_iter()
            .map(|def| def.name)
            .collect()
    }

    #[test]
    fn no_resources_capable_server_means_no_resource_tools() {
        assert!(names(&state_with(&[])).is_empty());
        let state = state_with(&[("plain", McpServerStatus::Ready, false)]);
        assert!(names(&state).is_empty());
        // Capable but not (yet, or any longer) ready: still nothing.
        let state = state_with(&[
            ("starting", McpServerStatus::Starting, true),
            ("stopped", McpServerStatus::Stopped, true),
        ]);
        assert!(names(&state).is_empty());
    }

    #[test]
    fn a_ready_resources_server_advertises_both_tools_naming_it() {
        let state = state_with(&[
            ("docs", McpServerStatus::Ready, true),
            ("plain", McpServerStatus::Ready, false),
            ("archive", McpServerStatus::Ready, true),
        ]);
        assert_eq!(names(&state), [LIST_MCP_RESOURCES, READ_MCP_RESOURCE]);
        let defs = resource_tool_definitions(&state);
        assert!(
            defs[0]
                .description
                .ends_with("Servers with resources: archive, docs."),
            "{}",
            defs[0].description
        );
        assert_eq!(
            defs[1].input_schema["required"],
            serde_json::json!(["server", "uri"])
        );
    }
}
