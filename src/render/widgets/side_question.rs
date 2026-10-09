//! The `/btw` side-question pane: the bottom zone while
//! `Focus::SideQuestion` holds.
//!
//! Earlier questions sit dimmed on top (the newest few, plus a count of any
//! older), then the question on view, then its answer, which scrolls. The
//! pane only reads `SideQuestions`; the keys that move it live in the reducer.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use super::chat::wrap_assistant_content;
use super::truncate_to_cells;
use crate::render::theme::Theme;
use mermaid_domain::side_question::{SIDE_QUESTION_RECENT_SHOWN, SideQuestions, SideStatus};

/// The smallest pane: borders, the question, and one answer row.
const MIN_HEIGHT: u16 = 4;

pub struct SideQuestionWidget<'a> {
    pub theme: &'a Theme,
    pub side: &'a SideQuestions,
}

/// The pane's rows for `width` (the full pane width): the fixed head (earlier
/// questions and the one on view) and the scrolling answer.
fn pane_lines(
    side: &SideQuestions,
    theme: &Theme,
    width: u16,
) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
    let Some(view) = side.view else {
        return (Vec::new(), Vec::new());
    };
    let Some(exchange) = side.exchanges.get(view.index) else {
        return (Vec::new(), Vec::new());
    };
    let colors = &theme.colors;
    let inner = width.saturating_sub(2);
    let text_width = usize::from(inner).saturating_sub(4).max(8);
    let dim = Style::new().fg(colors.text_meta.to_color());

    let mut head = Vec::new();
    let earlier = &side.exchanges[..view.index];
    let shown = earlier.len().min(SIDE_QUESTION_RECENT_SHOWN);
    let hidden = earlier.len() - shown;
    if hidden > 0 {
        head.push(Line::from(Span::styled(format!("+ {hidden} older"), dim)));
    }
    for ex in &earlier[earlier.len() - shown..] {
        head.push(Line::from(Span::styled(
            format!(
                "> {}",
                truncate_to_cells(&first_line(&ex.question), text_width)
            ),
            dim,
        )));
    }
    head.push(Line::from(vec![
        Span::styled("> ", Style::new().fg(colors.brand.to_color()).bold()),
        Span::styled(
            truncate_to_cells(&first_line(&exchange.question), text_width),
            Style::new()
                .fg(colors.text_primary.to_color())
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    let prefix_color = colors.text_meta.to_color();
    let mut body = if exchange.answer.is_empty() {
        Vec::new()
    } else {
        wrap_assistant_content(&exchange.answer, inner, "⎿", prefix_color, theme)
    };
    match &exchange.status {
        SideStatus::Answering if body.is_empty() => {
            body.push(Line::from(Span::styled("⎿ Answering…", dim)));
        },
        SideStatus::Answering | SideStatus::Done => {},
        SideStatus::Failed(reason) => body.push(Line::from(Span::styled(
            format!("⎿ The side question failed: {reason}"),
            Style::new().fg(colors.error.to_color()),
        ))),
    }
    (head, body)
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_string()
}

/// The pane's height for this frame: everything it holds, within
/// `[MIN_HEIGHT, max]`.
#[must_use]
pub fn side_question_height(side: &SideQuestions, theme: &Theme, width: u16, max: u16) -> u16 {
    let (head, body) = pane_lines(side, theme, width);
    let want = u16::try_from(2 + head.len() + body.len()).unwrap_or(u16::MAX);
    want.min(max).max(MIN_HEIGHT)
}

impl Widget for SideQuestionWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let Some(view) = self.side.view else {
            return;
        };
        let colors = &self.theme.colors;
        let count = self.side.exchanges.len();
        let footer = if count > 1 {
            format!(
                " Esc close · ↑↓ scroll · ←→ {} of {count} · c copy · x clear earlier ",
                view.index + 1
            )
        } else {
            " Esc close · ↑↓ scroll · c copy ".to_string()
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" /btw · side question · not added to the conversation ")
            .title_bottom(Line::from(footer))
            .border_style(Style::new().fg(colors.border.to_color()));
        let inner = block.inner(area);
        block.render(area, buf);

        let (head, body) = pane_lines(self.side, self.theme, area.width);
        let head_height = u16::try_from(head.len())
            .unwrap_or(u16::MAX)
            .min(inner.height.saturating_sub(1));
        let head_area = Rect {
            height: head_height,
            ..inner
        };
        // Keep the newest head rows (the question on view) when space is short.
        let skip = head.len().saturating_sub(usize::from(head_height));
        Paragraph::new(head.into_iter().skip(skip).collect::<Vec<_>>()).render(head_area, buf);

        let body_area = Rect {
            y: inner.y + head_height,
            height: inner.height - head_height,
            ..inner
        };
        let max_scroll = u16::try_from(body.len())
            .unwrap_or(u16::MAX)
            .saturating_sub(body_area.height);
        Paragraph::new(body)
            .scroll((view.scroll.min(max_scroll), 0))
            .render(body_area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mermaid_domain::side_question::SideOutcome;

    fn render_to_string(side: &SideQuestions, width: u16, height: u16) -> String {
        let theme = Theme::plain();
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        SideQuestionWidget {
            theme: &theme,
            side,
        }
        .render(area, &mut buf);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn shows_the_question_and_a_waiting_line() {
        let mut side = SideQuestions::default();
        side.ask("which config file?".to_string());
        let out = render_to_string(&side, 70, 6);
        assert!(out.contains("> which config file?"), "{out}");
        assert!(out.contains("Answering"), "{out}");
        assert!(out.contains("not added to the conversation"), "{out}");
    }

    #[test]
    fn lists_earlier_questions_and_the_answer() {
        let mut side = SideQuestions::default();
        for q in ["first", "second"] {
            let id = side.ask(q.to_string());
            side.push_chunk(id, &format!("answer to {q}"));
            side.finish(id, SideOutcome::Done { tried_tools: false });
        }
        let out = render_to_string(&side, 70, 8);
        assert!(out.contains("│> first"), "{out}");
        assert!(out.contains("> second"), "{out}");
        assert!(out.contains("answer to second"), "{out}");
        assert!(out.contains("2 of 2"), "{out}");
    }

    #[test]
    fn shows_a_failure() {
        let mut side = SideQuestions::default();
        let id = side.ask("q".to_string());
        side.finish(id, SideOutcome::Failed("offline".to_string()));
        let out = render_to_string(&side, 70, 6);
        assert!(out.contains("failed: offline"), "{out}");
    }

    #[test]
    fn height_grows_with_the_answer_and_stops_at_max() {
        let theme = Theme::plain();
        let mut side = SideQuestions::default();
        let id = side.ask("q".to_string());
        assert_eq!(side_question_height(&side, &theme, 60, 20), MIN_HEIGHT);
        side.push_chunk(id, &"line\n\n".repeat(40));
        assert_eq!(side_question_height(&side, &theme, 60, 20), 20);
    }
}
