pub mod cache;
pub mod markdown;
pub mod node;
pub mod theme;
pub mod widgets;
pub mod wrap;

pub use cache::{ChatState, FrameMemo, ImageClickTarget, StitchedMemo, UiCache};
pub use markdown::{MarkdownLine, parse_markdown, parse_markdown_inline};
pub use node::{BorderStyle, Constraint, FlexDirection, Line, Rect, Span, UiNode, Viewport};
pub use theme::{ColorValue, StyleToken, Theme, ThemeColors, ThemeToken};
pub use widgets::*;
pub use wrap::{
    hard_break_plain_token, hard_break_styled_word, line_hanging_indent, rendered_row_count,
    truncate_to_cells, wrap_styled_line, wrap_text_with_indent,
};

use mermaid_domain::{GenPhase, State, TurnState, UiMode};
use mermaid_model::models::ReasoningLevel;

fn is_exit_armed(state: &State) -> bool {
    state.ui.exit_armed_until.is_some_and(|d| state.now <= d)
}

fn is_rewind_armed(state: &State) -> bool {
    state
        .ui
        .esc_armed_at
        .is_some_and(|armed| (state.now - armed) <= chrono::Duration::milliseconds(1000))
}

fn compute_elapsed_secs(state: &State) -> u64 {
    let now_sys = std::time::SystemTime::from(state.now);
    let elapsed_since =
        |t: std::time::SystemTime| now_sys.duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
    match &state.turn {
        TurnState::Generating { started, .. } | TurnState::ExecutingTools { started, .. } => state
            .runtime
            .run_started
            .map_or_else(|| elapsed_since(*started), elapsed_since),
        TurnState::Compacting { started, .. } => elapsed_since(*started),
        TurnState::Cancelling { since, .. } => elapsed_since(*since),
        TurnState::Idle => 0,
    }
}

fn compute_tokens_display(state: &State) -> (usize, bool) {
    let committed = state.runtime.run_tokens;
    let live_child_tokens: usize = state.ui.live_tool_status.values().map(|l| l.tokens).sum();
    match &state.turn {
        TurnState::Generating { tokens, .. } => (committed.output_tokens + *tokens, true),
        TurnState::ExecutingTools { .. } => (
            committed.output_tokens + live_child_tokens,
            committed.contains_estimate || live_child_tokens > 0,
        ),
        TurnState::Compacting { .. } | TurnState::Cancelling { .. } | TurnState::Idle => {
            (committed.output_tokens, committed.contains_estimate)
        },
    }
}

fn build_input_or_palette_view(state: &State, cache: &UiCache, width: usize) -> UiNode {
    if state.ui.file_picker_open() {
        let matches: Vec<String> = state.ui.file_picker_matches.clone();
        build_file_picker_view(FilePickerProps {
            theme: &cache.theme,
            matches: &matches,
            selected_index: state.ui.file_picker_cursor.unwrap_or(0),
            loading: state.ui.project_files_loading,
        })
    } else if state.ui.input_buffer.starts_with('/') {
        let entries = mermaid_domain::slash_commands::filter_entries(
            &state.ui.input_buffer,
            &state.plugin_commands,
        );
        build_slash_palette_view(SlashPaletteProps {
            theme: &cache.theme,
            entries,
            selected_index: state.ui.palette_cursor.unwrap_or(0),
        })
    } else {
        build_input_view(InputProps {
            input: &state.ui.input_buffer,
            showing_command_hints: false,
            theme: &cache.theme,
            reasoning_active: state.session.reasoning != ReasoningLevel::None,
            exit_armed: is_exit_armed(state),
            rewind_armed: is_rewind_armed(state),
            width,
        })
    }
}

fn build_mode_pane(state: &State, cache: &UiCache, width: usize) -> UiNode {
    match &state.ui.mode {
        UiMode::ModelPicker {
            candidates,
            query,
            cursor,
            loading,
        } => {
            let current = &state.session.model_id;
            let matches = mermaid_domain::reducer::filter_model_choices(candidates, query);
            build_model_picker_view(ModelPickerProps {
                theme: &cache.theme,
                matches: &matches,
                query,
                cursor: *cursor,
                loading: *loading,
                current,
                width,
                height: MODEL_PICKER_HEIGHT as usize,
            })
        },
        UiMode::PlanConfig { cursor } => {
            let session_model = &state.session.model_id;
            let plan = &state.settings.plan;
            build_plan_config_view(PlanConfigProps {
                theme: &cache.theme,
                plan,
                session_model,
                cursor: *cursor,
            })
        },
        UiMode::ConversationList { candidates, cursor } => {
            build_conversation_list_view(ConversationListProps {
                theme: &cache.theme,
                candidates,
                cursor: *cursor,
                height: 10,
            })
        },
        UiMode::RewindPicker { candidates, cursor } => {
            build_rewind_picker_view(RewindPickerProps {
                theme: &cache.theme,
                candidates,
                cursor: *cursor,
                height: 10,
            })
        },
        UiMode::EditingInput | UiMode::ModelList => {
            build_input_or_palette_view(state, cache, width)
        },
    }
}

fn build_bottom_pane(state: &State, cache: &UiCache, viewport: Viewport) -> UiNode {
    let width = viewport.width as usize;
    let approval_item = state.pending_approval.front();
    let question_item = if approval_item.is_none() {
        state.pending_question.front()
    } else {
        None
    };

    if let Some(item) = approval_item {
        let options = if item.allowlist_scope.is_empty() {
            vec!["1. Yes".to_string(), "2. No  (Esc)".to_string()]
        } else {
            vec![
                "1. Yes".to_string(),
                format!("2. Yes, and don't ask again for `{}`", item.allowlist_scope),
                "3. No  (Esc)".to_string(),
            ]
        };
        build_approval_modal_view(
            ApprovalModalProps {
                theme: &cache.theme,
                title: format!("Approval required — {}  [{}]", item.tool, item.risk),
                body: item.prompt.as_str(),
                options,
                selected_index: Some(item.selected_option),
                accent: ThemeToken::Warning,
            },
            width,
        )
    } else if let Some(confirm) = &state.confirm {
        build_approval_modal_view(
            ApprovalModalProps {
                theme: &cache.theme,
                title: "Confirm".to_string(),
                body: confirm.prompt.as_str(),
                options: vec!["y. Yes".to_string(), "n. No  (Esc)".to_string()],
                selected_index: None,
                accent: ThemeToken::Warning,
            },
            width,
        )
    } else if let Some(q_set) = question_item {
        build_question_modal_view(q_set, &cache.theme, viewport.width)
    } else {
        build_mode_pane(state, cache, width)
    }
}

/// The primary pure view function: computes a declarative UI node tree from domain state.
#[must_use]
pub fn view(state: &State, cache: &mut UiCache, viewport: Viewport) -> UiNode {
    cache.apply_theme_diff(state.ui.theme, state.ui.no_color);
    cache.update_scroll_from_state(state);

    let width = viewport.width as usize;
    let bottom_node = build_bottom_pane(state, cache, viewport);

    let gen_status = match &state.turn {
        TurnState::ExecutingTools { .. } => GenerationStatus::RunningTools,
        TurnState::Compacting { .. } => GenerationStatus::Compacting,
        TurnState::Cancelling { .. } => GenerationStatus::Cancelling,
        TurnState::Generating { phase, .. } => match phase {
            GenPhase::Sending => GenerationStatus::Sending,
            GenPhase::Thinking => GenerationStatus::Thinking,
            GenPhase::Streaming => GenerationStatus::Streaming,
        },
        TurnState::Idle => GenerationStatus::Idle,
    };

    let elapsed_secs = compute_elapsed_secs(state);
    let (tokens_received, tokens_estimated) = compute_tokens_display(state);

    let status_lines = build_status_lines(widgets::StatusLineProps {
        status: gen_status,
        elapsed_secs,
        tokens_received,
        tokens_estimated,
        status_override: None,
        agents: &[],
        bg_available: false,
        task_headline: None,
        queued_messages: &state.ui.queued_messages,
        exit_armed: is_exit_armed(state),
        theme: &cache.theme,
        width: viewport.width,
    });

    let chat_messages = state.session.messages();
    let chat_node = build_chat_view(
        ChatProps {
            messages: chat_messages,
            theme: &cache.theme,
            content_key: state.session.conversation.revision(),
            show_reasoning: state.ui.show_reasoning,
            blink_on: true,
            today: state.now.date_naive(),
        },
        &mut cache.chat,
        width,
    );

    let working_dir = state.session.conversation.project_path.as_str();
    let status_node = build_status_view(StatusProps {
        theme: &cache.theme,
        working_dir,
        hostname: &cache.hostname,
        username: &cache.username,
        version: &cache.version,
        context_usage: state.session.context_usage.as_ref(),
        model_name: &state.session.model_id,
        reasoning_level: state.session.reasoning,
        requested_level: Some(state.session.reasoning),
        safety_mode: state.session.safety_mode,
        width,
    });

    let mut children = vec![chat_node];
    if !status_lines.is_empty() {
        children.push(UiNode::text(status_lines));
    }
    children.push(bottom_node);
    children.push(status_node);

    UiNode::vertical(
        children,
        vec![
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(2),
        ],
    )
}
