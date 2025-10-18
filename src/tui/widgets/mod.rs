// Gateway module for widgets - follows the Train Station Pattern
// All external access must go through this gateway

mod chat;
mod header;
mod input;
mod sidebar;
mod status;
mod status_line;

pub use chat::{ChatState, ChatWidget};
pub use header::HeaderWidget;
pub use input::{InputState, InputWidget};
pub use sidebar::{SidebarState, SidebarWidget};
pub use status::StatusWidget;
pub use status_line::StatusLineWidget;
