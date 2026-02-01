/// Operation state management
///
/// Handles operation modes, confirmations, and plan execution.

use crate::agents::AgentAction;
use crate::tui::mode::OperationMode;

/// Operation state - mode, confirmations, and plan execution
pub struct OperationState {
    /// Current operation mode (Normal, AcceptEdits, PlanMode, BypassAll)
    pub operation_mode: OperationMode,
    /// Flag for confirming destructive operations in BypassAll mode
    pub bypass_confirmed: bool,
    /// Track if FILE_READ feedback is pending
    pub pending_file_read: bool,
    /// Current step index during plan execution (0-based)
    pub plan_execution_index: Option<usize>,
    /// Track if plan mode was active when generation started
    pub plan_mode_active_for_generation: bool,
    /// Status text to show during file reading
    pub reading_file_status: Option<String>,
    /// Current confirmation state
    pub confirmation_state: Option<ConfirmationState>,
    /// Accumulated tool calls during streaming (persists across process_stream_chunks calls)
    pub accumulated_tool_calls: Vec<crate::models::ToolCall>,
}

impl OperationState {
    /// Create a new OperationState with default values
    pub fn new() -> Self {
        Self {
            operation_mode: OperationMode::default(),
            bypass_confirmed: false,
            pending_file_read: false,
            plan_execution_index: None,
            plan_mode_active_for_generation: false,
            reading_file_status: None,
            confirmation_state: None,
            accumulated_tool_calls: Vec::new(),
        }
    }
}

impl Default for OperationState {
    fn default() -> Self {
        Self::new()
    }
}

/// State for action confirmation
#[derive(Debug, Clone)]
pub struct ConfirmationState {
    pub action: AgentAction,
    pub action_description: String,
    pub preview_lines: Vec<String>,  // First few lines for preview
    pub file_info: Option<FileInfo>, // Size, path, overwrite status
    pub allow_always: bool,          // Can user select "always approve"?
}

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub path: String,
    pub size: usize,
    pub exists: bool,
    pub language: Option<String>,
}
