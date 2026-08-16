pub mod approval;
pub mod chat;
pub mod conversation_list;
pub mod file_picker;
pub mod input;
pub mod model_picker;
pub mod plan_config;
pub mod question;
pub mod rewind_picker;
pub mod slash_palette;
pub mod status;
pub mod status_line;
pub mod tasks;

pub use approval::{ApprovalModalProps, build_approval_modal_view};
pub use chat::{ChatProps, build_chat_lines, build_chat_view, wrap_assistant_content};
pub use conversation_list::{ConversationListProps, build_conversation_list_view};
pub use file_picker::{FilePickerProps, build_file_picker_view};
pub use input::{
    InputProps, InputState, build_input_view, rendered_row_count, wrap_input_with_prompt,
};
pub use model_picker::{
    MODEL_PICKER_HEIGHT, MODEL_PICKER_VISIBLE_ROWS, ModelPickerProps, build_model_picker_view,
};
pub use plan_config::{
    PLAN_CONFIG_HEIGHT, PLAN_CONFIG_ROWS, PlanConfigProps, build_plan_config_view, plan_config_rows,
};
pub use question::{
    build_preview_lines, build_question_lines, build_question_modal_view, question_modal_height,
};
pub use rewind_picker::{RewindPickerProps, build_rewind_picker_view};
pub use slash_palette::{SlashPaletteProps, build_slash_palette_view};
pub use status::{StatusProps, build_status_view, format_token_status};
pub use status_line::{AgentPanelRow, GenerationStatus, StatusLineProps, build_status_lines};
pub use tasks::{build_task_lines, build_tasks_view, tasks_height, tasks_visible};

pub struct QuestionModalProps<'a> {
    pub theme: &'a crate::theme::Theme,
    pub set: &'a mermaid_model::question::PendingQuestionSet,
    pub width: u16,
}
