//! Stateless TUI widgets.
//!
//! Each widget takes explicit props; the compose function in
//! `render::mod` pulls those props from `State` per frame. No widget
//! holds a reference to any god-object.

mod approval;
mod attachment;
mod chat;
mod conversation_list;
mod input;
mod slash_palette;
mod status;
mod status_banner;
mod status_line;

pub use approval::ApprovalModalWidget;
pub use attachment::AttachmentWidget;
pub use chat::{ChatState, ChatWidget, ImageClickTarget};
pub use conversation_list::ConversationListWidget;
pub use input::{InputState, InputWidget};
pub use slash_palette::SlashPaletteWidget;
pub use status::StatusWidget;
pub use status_banner::StatusBannerWidget;
pub use status_line::StatusLineWidget;

/// Local-to-render-layer generation phase enum. The compose function
/// converts from `domain::TurnState` + `domain::GenPhase` into one of
/// these four states; widgets render off this local view so they
/// don't need to pattern-match the full domain enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationStatus {
    Idle,
    Sending,
    Thinking,
    Streaming,
    RunningTools,
    Compacting,
    Cancelling,
}

impl GenerationStatus {
    pub fn display_text(&self) -> &str {
        match self {
            GenerationStatus::Idle => "Idle",
            GenerationStatus::Sending => "Sending",
            GenerationStatus::Thinking => "Thinking",
            GenerationStatus::Streaming => "Streaming",
            GenerationStatus::RunningTools => "Running tools",
            GenerationStatus::Compacting => "Compacting",
            GenerationStatus::Cancelling => "Cancelling",
        }
    }

    /// Convert from the reducer's typed turn state. `TurnState::Idle`
    /// maps to `Idle`; `Generating.phase` maps 1:1; every other
    /// active variant maps to `Streaming` (the status-line widget
    /// doesn't distinguish beyond the basic upstream/downstream
    /// arrow).
    pub fn from_turn(turn: &crate::domain::TurnState) -> Self {
        use crate::domain::{GenPhase, TurnState};
        match turn {
            TurnState::Idle => GenerationStatus::Idle,
            TurnState::Generating { phase, .. } => match phase {
                GenPhase::Sending => GenerationStatus::Sending,
                GenPhase::Thinking => GenerationStatus::Thinking,
                GenPhase::Streaming => GenerationStatus::Streaming,
            },
            TurnState::ExecutingTools { .. } => GenerationStatus::RunningTools,
            TurnState::Compacting { .. } => GenerationStatus::Compacting,
            TurnState::Cancelling { .. } => GenerationStatus::Cancelling,
        }
    }
}
