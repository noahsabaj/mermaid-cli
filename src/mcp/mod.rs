/// MCP (Model Context Protocol) client integration — Gateway
///
/// Connects to external MCP servers and exposes their tools (and, where a
/// server offers them, its resources and prompts) to the model and the user.
/// Servers are configured in config.toml and spawned as child processes.
pub mod add;
mod client;
pub mod manager_ref;
pub mod oauth;
mod param_headers;
mod registry;
pub mod sanitize;
mod server_manager;
mod transport;
mod transport_http;

pub use add::{add_http_server, add_server, remove_server};
pub use client::{
    ContentBlock, McpClient, McpResource, McpToolDef, McpToolResult, ResourceContents,
};
pub use server_manager::McpServerManager;
