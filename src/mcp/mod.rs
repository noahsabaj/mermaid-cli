/// MCP (Model Context Protocol) client integration — Gateway
///
/// Connects to external MCP servers and exposes their tools to the model.
/// Servers are configured in config.toml and spawned as child processes.
pub mod add;
mod client;
pub mod manager_ref;
mod registry;
mod server_manager;
mod transport;

pub use add::{add_server, remove_server};
pub use client::{ContentBlock, McpClient, McpToolDef, McpToolResult};
pub use server_manager::McpServerManager;
