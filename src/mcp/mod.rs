/// MCP (Model Context Protocol) client integration — Gateway
///
/// Connects to external MCP servers and exposes their tools to the model.
/// Servers are configured in config.toml and spawned as child processes.
pub mod add;
mod client;
pub mod manager_ref;
mod registry;
pub mod sanitize;
mod server_manager;
mod transport;
mod transport_http;

pub use add::{add_http_server, add_server, remove_server};
pub use client::{ContentBlock, McpClient, McpToolDef, McpToolResult};
pub use server_manager::McpServerManager;
