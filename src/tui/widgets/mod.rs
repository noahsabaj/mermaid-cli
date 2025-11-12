// Gateway module for widgets - follows the Train Station Pattern
// All external access must go through this gateway

mod chat;
mod input;
mod status;
mod status_line;

pub use chat::{ChatState, ChatWidget};
pub use input::{InputState, InputWidget};
pub use status::StatusWidget;
pub use status_line::StatusLineWidget;
