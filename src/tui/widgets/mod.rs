// Gateway module for widgets - follows the Train Station Pattern
// All external access must go through this gateway

mod chat;
mod input;
mod status;
mod header;
mod sidebar;

pub use chat::{ChatWidget, ChatState};
pub use input::{InputWidget, InputState};
pub use status::StatusWidget;
pub use header::HeaderWidget;
pub use sidebar::{SidebarWidget, SidebarState};
