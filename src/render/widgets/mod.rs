//! Ported v0.6 widgets, rewired to read from v0.7 `State`.
//!
//! Each widget takes explicit props (not `&App`); the compose
//! function in `render::mod` pulls those props from `State` per
//! frame. No widget holds a reference to `App`.

mod attachment;
mod chat;
mod input;
mod slash_palette;
mod status;
mod status_line;

pub use attachment::AttachmentWidget;
pub use chat::{ChatState, ChatWidget, ImageClickTarget};
pub use input::{InputState, InputWidget};
pub use slash_palette::SlashPaletteWidget;
pub use status::StatusWidget;
pub use status_line::StatusLineWidget;

/// Local-to-render-layer generation phase enum. Mirrors v0.6's
/// `tui::state::GenerationStatus` exactly so the ported widget code
/// can stay byte-identical; the compose function converts from
/// `domain::TurnState` + `domain::GenPhase`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationStatus {
    Idle,
    Sending,
    Thinking,
    Streaming,
}

impl GenerationStatus {
    pub fn display_text(&self) -> &str {
        match self {
            GenerationStatus::Idle => "Idle",
            GenerationStatus::Sending => "Sending",
            GenerationStatus::Thinking => "Thinking",
            GenerationStatus::Streaming => "Streaming",
        }
    }

    /// Convert from the reducer's typed turn state. `TurnState::Idle`
    /// maps to `Idle`; `Generating.phase` maps 1:1;
    /// `ExecutingTools` / `RunningSubagents` / `Cancelling` all map to
    /// `Streaming` (the status-line widget doesn't distinguish
    /// beyond the basic upstream/downstream arrow).
    pub fn from_turn(turn: &crate::domain::TurnState) -> Self {
        use crate::domain::{GenPhase, TurnState};
        match turn {
            TurnState::Idle => GenerationStatus::Idle,
            TurnState::Generating { phase, .. } => match phase {
                GenPhase::Sending => GenerationStatus::Sending,
                GenPhase::Thinking => GenerationStatus::Thinking,
                GenPhase::Streaming => GenerationStatus::Streaming,
            },
            TurnState::ExecutingTools { .. }
            | TurnState::RunningSubagents { .. }
            | TurnState::Cancelling { .. } => GenerationStatus::Streaming,
        }
    }
}
