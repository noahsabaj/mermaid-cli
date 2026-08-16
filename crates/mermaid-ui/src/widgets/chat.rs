use chrono::NaiveDate;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::cache::ImageClickTarget;
use crate::markdown::parse_markdown;
use crate::node::{Line, Span, UiNode};
use crate::theme::{StyleToken, Theme, ThemeToken};
use crate::wrap::{truncate_to_cells, wrap_styled_line};
use mermaid_domain::{ActionDetails, ActionDisplay, ActionResult};
use mermaid_model::models::{ChatMessage, ChatMessageKind, MessageRole};
use mermaid_model::utils::format_relative_timestamp;

#[derive(Debug, Clone)]
pub struct ChatProps<'a> {
    pub messages: &'a [ChatMessage],
    pub theme: &'a Theme,
    pub content_key: u64,
    pub show_reasoning: bool,
    pub blink_on: bool,
    pub today: NaiveDate,
}

fn wrap_preformatted(line: Line, width: usize, indent: usize) -> Vec<Line> {
    if width == 0 {
        return vec![line];
    }
    let total: usize = line.spans.iter().map(Span::width).sum();
    if total <= width {
        return vec![line];
    }

    let mut out: Vec<Line> = Vec::new();
    let mut cur: Vec<Span> = Vec::new();
    let mut cur_w = 0usize;
    let mut on_first = true;

    for span in line.spans {
        let style = span.style;
        let mut buf = String::new();
        for ch in span.content.chars() {
            let cw = ch.width().unwrap_or(0);
            let floor = if on_first { 0 } else { indent };
            if cur_w + cw > width && cur_w > floor {
                if !buf.is_empty() {
                    cur.push(Span::styled(std::mem::take(&mut buf), style));
                }
                out.push(Line::from(std::mem::take(&mut cur)));
                on_first = false;
                cur.push(Span::raw(" ".repeat(indent)));
                cur_w = indent;
            }
            buf.push(ch);
            cur_w += cw;
        }
        if !buf.is_empty() {
            cur.push(Span::styled(buf, style));
        }
    }
    if !cur.is_empty() {
        out.push(Line::from(cur));
    }
    if out.is_empty() {
        vec![Line::raw("")]
    } else {
        out
    }
}

pub fn wrap_assistant_content(
    content: &str,
    content_width: usize,
    role_prefix: &str,
    theme: &Theme,
) -> Vec<Line> {
    let md_width = content_width.saturating_sub(2);
    let parsed = parse_markdown(content, theme, md_width);

    let mut out: Vec<Line> = Vec::new();
    for (line_idx, parsed_line) in parsed.into_iter().enumerate() {
        let preformatted = parsed_line.preformatted;
        let continuation = if preformatted {
            2
        } else {
            2 + crate::markdown::line_hanging_indent(&parsed_line.line)
        };

        let mut spans = if line_idx == 0 {
            vec![Span::styled(
                format!("{role_prefix} "),
                StyleToken::new().fg(ThemeToken::AssistantMessage).bold(),
            )]
        } else {
            vec![Span::raw("  ")]
        };
        spans.extend(parsed_line.line.spans);
        let new_line = Line::from(spans);

        if preformatted {
            out.extend(wrap_preformatted(new_line, content_width, 2));
        } else {
            out.extend(wrap_styled_line(new_line, content_width, continuation));
        }
    }
    out
}

fn render_user_message(
    msg: &ChatMessage,
    msg_idx: usize,
    click_map: &mut Vec<(u16, ImageClickTarget)>,
    line_offset: usize,
    content_width: usize,
    _theme: &Theme,
    today: NaiveDate,
) -> Vec<Line> {
    let mut lines = Vec::new();
    let text = &msg.content;
    let timestamp = format_relative_timestamp(msg.timestamp, today);
    let role_prefix = "●";
    let role_style = StyleToken::new().fg(ThemeToken::UserMessage).bold();
    let bg_style = StyleToken::new().bg(ThemeToken::UserMessageBackground);

    let text_lines: Vec<&str> = text.lines().collect();
    if text_lines.is_empty() {
        return lines;
    }

    for (i, line) in text_lines.iter().enumerate() {
        if i == 0 {
            let mut spans = vec![
                Span::styled(format!("{role_prefix} "), role_style),
                Span::styled(
                    line.to_string(),
                    StyleToken::new().fg(ThemeToken::TextPrimary),
                ),
            ];
            let used: usize = spans.iter().map(Span::width).sum();
            let ts_width = timestamp.width();
            if content_width > used + ts_width + 1 {
                let pad = content_width - used - ts_width;
                spans.push(Span::raw(" ".repeat(pad)));
                spans.push(Span::styled(
                    timestamp.clone(),
                    StyleToken::new().fg(ThemeToken::TextMeta),
                ));
            }
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    line.to_string(),
                    StyleToken::new().fg(ThemeToken::TextPrimary),
                ),
            ]));
        }
    }

    if let Some(imgs) = &msg.images {
        for (img_idx, _) in imgs.iter().enumerate() {
            let line_num = line_offset + lines.len();
            click_map.push((
                u16::try_from(line_num).unwrap_or(u16::MAX),
                ImageClickTarget {
                    message_index: msg_idx,
                    image_index: img_idx,
                    image_number: None,
                },
            ));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("[Image #{}]", img_idx + 1),
                    StyleToken::new().fg(ThemeToken::Info).underline(),
                ),
            ]));
        }
    }

    lines
        .into_iter()
        .map(|mut l| {
            for s in &mut l.spans {
                s.style.bg = bg_style.bg;
            }
            l
        })
        .collect()
}

fn render_action_display(
    action: &ActionDisplay,
    _theme: &Theme,
    width: usize,
    blink_on: bool,
) -> Vec<Line> {
    let mut lines = Vec::new();
    let action_color = match action.action_type.as_str() {
        "Write" | "Update" => ThemeToken::Success,
        "Delete" => ThemeToken::Warning,
        _ => ThemeToken::Info,
    };

    let dot_style = if matches!(action.result, ActionResult::Running) && !blink_on {
        StyleToken::new().fg(ThemeToken::TextDisabled).bold()
    } else {
        StyleToken::new().fg(action_color).bold()
    };

    lines.push(Line::from(vec![
        Span::styled("● ", dot_style),
        Span::styled(
            format!(
                "{}({})",
                action.action_type,
                truncate_to_cells(&action.target, width.saturating_sub(10))
            ),
            StyleToken::new().fg(action_color).bold(),
        ),
    ]));

    match &action.result {
        ActionResult::Running => {},
        ActionResult::Success { .. } => {
            let result_msg = match &action.details {
                ActionDetails::FileContent { line_count, .. } => {
                    format!(
                        "{} {} written",
                        line_count,
                        if *line_count == 1 { "line" } else { "lines" }
                    )
                },
                ActionDetails::Diff { summary, .. } => summary.clone(),
                ActionDetails::Preview { text, .. } => text.clone(),
                ActionDetails::Simple => String::new(),
            };
            if !result_msg.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("  ⎿ ", StyleToken::new().fg(action_color)),
                    Span::styled(result_msg, StyleToken::new().fg(ThemeToken::TextSecondary)),
                ]));
            }
        },
        ActionResult::Error { error } => {
            lines.push(Line::from(vec![
                Span::styled(
                    "  ⎿ error: ",
                    StyleToken::new().fg(ThemeToken::Error).bold(),
                ),
                Span::styled(
                    truncate_to_cells(error, width.saturating_sub(12)),
                    StyleToken::new().fg(ThemeToken::Error),
                ),
            ]));
        },
    }

    lines
}

#[must_use]
pub fn build_chat_lines(
    props: ChatProps<'_>,
    chat_state: &mut crate::cache::ChatState,
    content_width: usize,
) -> Vec<Line> {
    let mut lines: Vec<Line> = Vec::new();
    chat_state.image_click_map.clear();

    for (idx, msg) in props.messages.iter().enumerate() {
        if matches!(msg.role, MessageRole::Tool) {
            continue;
        }

        if matches!(msg.kind, ChatMessageKind::RunSummary) {
            lines.push(Line::from(Span::styled(
                format!("  {}", msg.content),
                StyleToken::new().fg(ThemeToken::TextMeta),
            )));
            lines.push(Line::from(""));
            continue;
        }

        if matches!(
            msg.kind,
            ChatMessageKind::RecoveryNudge | ChatMessageKind::ContextMarker
        ) {
            continue;
        }

        if matches!(msg.role, MessageRole::System) {
            lines.push(Line::from(Span::styled(
                format!("  {}", msg.content),
                StyleToken::new().fg(ThemeToken::TextMeta),
            )));
            lines.push(Line::from(""));
            continue;
        }

        if matches!(msg.role, MessageRole::User) {
            let offset = lines.len();
            lines.extend(render_user_message(
                msg,
                idx,
                &mut chat_state.image_click_map,
                offset,
                content_width,
                props.theme,
                props.today,
            ));
            lines.push(Line::from(""));
            continue;
        }

        // Assistant message
        if !msg.content.is_empty() {
            let wrapped = wrap_assistant_content(&msg.content, content_width, "●", props.theme);
            lines.extend(wrapped);
            lines.push(Line::from(""));
        }

        // Actions
        for action in &msg.actions {
            lines.extend(render_action_display(
                action,
                props.theme,
                content_width,
                props.blink_on,
            ));
            lines.push(Line::from(""));
        }
    }

    lines
}

#[must_use]
pub fn build_chat_view(
    props: ChatProps<'_>,
    chat_state: &mut crate::cache::ChatState,
    content_width: usize,
) -> UiNode {
    let lines = build_chat_lines(props, chat_state, content_width);
    UiNode::text(lines)
}
