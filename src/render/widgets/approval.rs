//! Bottom-zone modal prompt: a title, a body (the command / path / question),
//! and numbered options. Used for inline tool-approval prompts (`ask` mode +
//! Auto-mode escalations) and the `/clear`-style confirmation.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::render::theme::Theme;

pub struct ApprovalModalWidget<'a> {
    pub theme: &'a Theme,
    pub title: String,
    /// Body text — may be multi-line (the command, plus any classifier reason).
    pub body: &'a str,
    /// The numbered option lines, e.g. `"1. Yes"`.
    pub options: Vec<String>,
    /// Highlighted option for arrow-key navigation, if the modal supports it
    /// (`None` for plain confirmations that only take y/n).
    pub selected_index: Option<usize>,
    /// Border + title accent.
    pub accent: Color,
}

impl<'a> Widget for ApprovalModalWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.title)
            .border_style(Style::default().fg(self.accent));

        let inner_width = area.width.saturating_sub(4).max(8) as usize;
        let mut lines: Vec<Line<'_>> = Vec::new();
        for raw in self.body.lines() {
            lines.push(Line::from(Span::styled(
                truncate(raw, inner_width),
                Style::default().fg(Color::White),
            )));
        }
        lines.push(Line::from(""));
        for (idx, opt) in self.options.iter().enumerate() {
            let style = if self.selected_index == Some(idx) {
                // Highlighted row — mirrors the slash palette's selection style.
                Style::default()
                    .fg(self.theme.colors.text_highlight.to_color())
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            };
            lines.push(Line::from(Span::styled(format!("  {}", opt), style)));
        }
        Paragraph::new(lines).block(block).render(area, buf);
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut = s.floor_char_boundary(max);
        format!("{}…", &s[..cut])
    }
}
