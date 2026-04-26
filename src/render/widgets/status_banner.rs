//! One-line banner for transient `state.status` messages.
//!
//! The reducer sets `state.status` for slash-command feedback
//! (`/model`, `/reasoning`, unknown command), MCP server errors, model
//! pull progress, and the `/help` hint. Before F9 none of it was
//! rendered — the render zone for status was only allocated while
//! `is_busy()`, and even then it showed the generation-phase line, not
//! `state.status`.
//!
//! This widget paints one line, color-keyed by `StatusKind`. Rendered
//! in `render::mod` whenever `state.status.is_some()`. Auto-dismiss
//! still flows through the existing `Cmd::DismissStatusAfter` →
//! `Msg::StatusDismiss` path.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::domain::{StatusKind, StatusLine};
use crate::render::theme::Theme;

pub struct StatusBannerWidget<'a> {
    pub theme: &'a Theme,
    pub status: &'a StatusLine,
}

impl<'a> Widget for StatusBannerWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        // Color by severity. Persistent uses the text-disabled gray so
        // it reads as ambient rather than alarm.
        let fg = match self.status.kind {
            StatusKind::Info => self.theme.colors.info.to_color(),
            StatusKind::Warn => self.theme.colors.warning.to_color(),
            StatusKind::Error => self.theme.colors.error.to_color(),
            StatusKind::Persistent => self.theme.colors.text_disabled.to_color(),
        };

        // Leading glyph mirrors `chat::render_actions` style — a bullet
        // sized to match and colored by severity.
        let glyph = match self.status.kind {
            StatusKind::Error => "✗ ",
            StatusKind::Warn => "! ",
            _ => "● ",
        };

        // Truncate on char boundary so CJK / emoji don't panic. One row
        // leaves 2 cells of glyph + 1 space of breathing room after,
        // so we cap the text at `width - 3` cells. Byte-count
        // truncation is close-enough for a status banner; exact cell
        // width would require unicode_width, which this widget doesn't
        // need to pull in.
        let max_body = area.width.saturating_sub(3) as usize;
        let text = &self.status.text;
        let truncated = if text.len() > max_body {
            let cut = text.floor_char_boundary(max_body.saturating_sub(1));
            format!("{}…", &text[..cut])
        } else {
            text.to_string()
        };

        let line = Line::from(vec![
            Span::styled(glyph, Style::new().fg(fg).add_modifier(Modifier::BOLD)),
            Span::styled(truncated, Style::new().fg(Color::White)),
        ]);
        Paragraph::new(line).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::theme::Theme;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render_to_string(banner: StatusBannerWidget<'_>, width: u16) -> String {
        let backend = TestBackend::new(width, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, width, 1);
                banner.render(area, f.buffer_mut());
            })
            .expect("draw");
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for x in 0..buf.area.width {
            out.push_str(buf[(x, 0)].symbol());
        }
        out
    }

    #[test]
    fn info_status_renders_bullet_and_text() {
        let theme = Theme::dark();
        let status = StatusLine {
            text: "Current model: ollama/qwen3-coder:30b".to_string(),
            kind: StatusKind::Info,
            shown_at: std::time::SystemTime::now(),
        };
        let banner = StatusBannerWidget {
            theme: &theme,
            status: &status,
        };
        let rendered = render_to_string(banner, 80);
        assert!(rendered.contains("●"));
        assert!(rendered.contains("ollama/qwen3-coder:30b"));
    }

    #[test]
    fn error_status_uses_error_glyph() {
        let theme = Theme::dark();
        let status = StatusLine {
            text: "MCP server foo errored: exit 1".to_string(),
            kind: StatusKind::Error,
            shown_at: std::time::SystemTime::now(),
        };
        let banner = StatusBannerWidget {
            theme: &theme,
            status: &status,
        };
        let rendered = render_to_string(banner, 80);
        assert!(rendered.contains("✗"));
        assert!(rendered.contains("exit 1"));
    }

    /// Long messages truncate on a char boundary. CJK content must not
    /// panic — the underlying `floor_char_boundary` handles the byte
    /// alignment; this test pins the behavior.
    #[test]
    fn long_cjk_text_truncates_without_panic() {
        let theme = Theme::dark();
        // 30 CJK chars = 90 bytes, each 2 display cells = 60 cells.
        let text: String = "你好世界".repeat(30);
        let status = StatusLine {
            text,
            kind: StatusKind::Info,
            shown_at: std::time::SystemTime::now(),
        };
        let banner = StatusBannerWidget {
            theme: &theme,
            status: &status,
        };
        // Pretend a narrow terminal — must not panic and must produce
        // *something*.
        let _ = render_to_string(banner, 10);
    }
}
