//! Ratatui backend adapter for `mermaid_ui::UiNode`.

use mermaid_ui::node::{BorderStyle, Constraint, FlexDirection, Line, Span, UiNode};
use mermaid_ui::theme::{StyleToken, Theme, ThemeToken};
use ratatui::buffer::Buffer;
use ratatui::layout::{
    Constraint as RConstraint, Direction as RDirection, Layout as RLayout, Rect as RRect,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as RLine, Span as RSpan};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::render::theme::ColorValueExt;

#[must_use]
pub fn theme_token_to_color(token: ThemeToken, theme: &Theme) -> Color {
    token.resolve(theme).to_color()
}

#[must_use]
pub fn style_token_to_ratatui(style: &StyleToken, theme: &Theme) -> Style {
    let mut r_style = Style::default();
    if let Some(fg) = style.fg {
        r_style = r_style.fg(theme_token_to_color(fg, theme));
    }
    if let Some(bg) = style.bg {
        r_style = r_style.bg(theme_token_to_color(bg, theme));
    }
    if style.bold {
        r_style = r_style.add_modifier(Modifier::BOLD);
    }
    if style.dim {
        r_style = r_style.add_modifier(Modifier::DIM);
    }
    if style.italic {
        r_style = r_style.add_modifier(Modifier::ITALIC);
    }
    if style.underline {
        r_style = r_style.add_modifier(Modifier::UNDERLINED);
    }
    if style.reversed {
        r_style = r_style.add_modifier(Modifier::REVERSED);
    }
    if style.strikethrough {
        r_style = r_style.add_modifier(Modifier::CROSSED_OUT);
    }
    r_style
}

#[must_use]
pub fn span_to_ratatui(span: &Span, theme: &Theme) -> RSpan<'static> {
    RSpan::styled(
        span.content.to_string(),
        style_token_to_ratatui(&span.style, theme),
    )
}

#[must_use]
pub fn line_to_ratatui(line: &Line, theme: &Theme) -> RLine<'static> {
    let spans: Vec<_> = line
        .spans
        .iter()
        .map(|s| span_to_ratatui(s, theme))
        .collect();
    RLine::from(spans)
}

#[must_use]
pub fn constraint_to_ratatui(c: &Constraint) -> RConstraint {
    match c {
        Constraint::Length(len) => RConstraint::Length(*len),
        Constraint::Percentage(pct) => RConstraint::Percentage(*pct),
        Constraint::Fill(f) => RConstraint::Fill(*f),
        Constraint::Min(min) => RConstraint::Min(*min),
        Constraint::Max(max) => RConstraint::Max(*max),
        Constraint::Ratio(a, b) => RConstraint::Ratio(*a, *b),
    }
}

pub fn render_ui_node(node: &UiNode, area: RRect, buf: &mut Buffer, theme: &Theme) {
    match node {
        UiNode::Empty => {},
        UiNode::Text { lines, wrap: _ } => {
            let r_lines: Vec<_> = lines.iter().map(|l| line_to_ratatui(l, theme)).collect();
            Paragraph::new(r_lines).render(area, buf);
        },
        UiNode::Container {
            direction,
            children,
            constraints,
            border,
            border_token,
            title,
            bg,
        } => {
            let mut block = match border {
                BorderStyle::None => Block::default(),
                BorderStyle::Plain
                | BorderStyle::Rounded
                | BorderStyle::Double
                | BorderStyle::Focused => Block::default().borders(Borders::ALL),
                BorderStyle::TopBottom => Block::default().borders(Borders::TOP | Borders::BOTTOM),
                BorderStyle::Left => Block::default().borders(Borders::LEFT),
            };

            if let Some(bc) = border_token {
                block = block.border_style(Style::default().fg(theme_token_to_color(*bc, theme)));
            }
            if let Some(bg_token) = bg {
                block = block.style(Style::default().bg(theme_token_to_color(*bg_token, theme)));
            }
            if let Some(t) = title {
                block = block.title(t.clone());
            }

            let inner_area = block.inner(area);
            block.render(area, buf);

            if children.is_empty() {
                return;
            }

            let r_dir = match direction {
                FlexDirection::Vertical => RDirection::Vertical,
                FlexDirection::Horizontal => RDirection::Horizontal,
            };

            let r_constraints: Vec<_> = if constraints.len() == children.len() {
                constraints.iter().map(constraint_to_ratatui).collect()
            } else {
                children.iter().map(|_| RConstraint::Fill(1)).collect()
            };

            let chunks = RLayout::default()
                .direction(r_dir)
                .constraints(r_constraints)
                .split(inner_area);

            for (idx, child) in children.iter().enumerate() {
                if idx < chunks.len() {
                    render_ui_node(child, chunks[idx], buf, theme);
                }
            }
        },
    }
}
