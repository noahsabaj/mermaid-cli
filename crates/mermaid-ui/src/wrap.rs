use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::node::{Line, Span};
use crate::theme::{StyleToken, ThemeToken};

/// Truncate `s` to `width` display cells, appending `…` when it doesn't fit.
/// Cell-accurate (CJK/emoji safe).
#[must_use]
pub fn truncate_to_cells(s: &str, width: usize) -> String {
    if UnicodeWidthStr::width(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let budget = width - 1; // leave a cell for the ellipsis
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Hard-break a single over-long token into the plain-text line accumulator,
/// splitting at char boundaries (UTF-8-safe, display-cell aware).
pub fn hard_break_plain_token(
    token: &str,
    out: &mut Vec<String>,
    current_line: &mut String,
    current_length: &mut usize,
    width: usize,
    continuation_indent: usize,
    initial_budget: usize,
) {
    let cont_budget = width.saturating_sub(continuation_indent).max(1);
    let mut line_budget = initial_budget.max(1);

    if *current_length > 0 {
        out.push(std::mem::take(current_line));
        current_line.push_str(&" ".repeat(continuation_indent));
        *current_length = 0;
        line_budget = cont_budget;
    }

    for ch in token.chars() {
        let cw = ch.width().unwrap_or(0);
        if *current_length + cw > line_budget && *current_length > 0 {
            out.push(std::mem::take(current_line));
            current_line.push_str(&" ".repeat(continuation_indent));
            *current_length = 0;
            line_budget = cont_budget;
        }
        current_line.push(ch);
        *current_length += cw;
    }
}

/// Wrap text with hanging indent support.
#[must_use]
pub fn wrap_text_with_indent(
    text: &str,
    width: usize,
    first_line_indent: usize,
    continuation_indent: usize,
) -> Vec<String> {
    let mut wrapped_lines = Vec::new();

    for (line_idx, line) in text.lines().enumerate() {
        if line.is_empty() {
            wrapped_lines.push(String::new());
            continue;
        }

        let current_indent = if line_idx == 0 {
            first_line_indent
        } else {
            continuation_indent
        };
        let available_width = width.saturating_sub(current_indent);

        if available_width == 0 {
            wrapped_lines.push(" ".repeat(current_indent));
            continue;
        }

        let words: Vec<&str> = line.split_whitespace().collect();
        if words.is_empty() {
            wrapped_lines.push(" ".repeat(current_indent));
            continue;
        }

        let mut current_line = String::with_capacity(width);
        current_line.push_str(&" ".repeat(current_indent));
        let mut current_length = 0;

        for (word_idx, word) in words.iter().enumerate() {
            let word_width = word.width();

            if word_idx == 0 {
                if word_width <= available_width {
                    current_line.push_str(word);
                    current_length = word_width;
                } else {
                    hard_break_plain_token(
                        word,
                        &mut wrapped_lines,
                        &mut current_line,
                        &mut current_length,
                        width,
                        continuation_indent,
                        available_width,
                    );
                }
            } else if current_length + 1 + word_width <= available_width {
                current_line.push(' ');
                current_line.push_str(word);
                current_length += 1 + word_width;
            } else if word_width <= available_width {
                wrapped_lines.push(current_line);
                current_line = String::with_capacity(width);
                current_line.push_str(&" ".repeat(continuation_indent));
                current_line.push_str(word);
                current_length = word_width;
            } else {
                hard_break_plain_token(
                    word,
                    &mut wrapped_lines,
                    &mut current_line,
                    &mut current_length,
                    width,
                    continuation_indent,
                    available_width,
                );
            }
        }

        if !current_line.trim().is_empty() {
            wrapped_lines.push(current_line);
        }
    }

    wrapped_lines
}

/// Hard-break a single over-long word into the styled line accumulator.
pub fn hard_break_styled_word(
    fragments: &[(String, StyleToken)],
    result_lines: &mut Vec<Line>,
    current_line_spans: &mut Vec<Span>,
    current_line_width: &mut usize,
    continuation_indent: usize,
    continuation_capacity: usize,
    mut line_capacity: usize,
) {
    for (text, style) in fragments {
        let mut buf = String::new();
        for ch in text.chars() {
            let cw = ch.width().unwrap_or(0);
            if *current_line_width + cw > line_capacity && *current_line_width > 0 {
                if !buf.is_empty() {
                    current_line_spans.push(Span::styled(std::mem::take(&mut buf), *style));
                }
                result_lines.push(Line::from(std::mem::take(current_line_spans)));
                current_line_spans.push(Span::raw(" ".repeat(continuation_indent)));
                *current_line_width = 0;
                line_capacity = continuation_capacity.max(1);
            }
            buf.push(ch);
            *current_line_width += cw;
        }
        if !buf.is_empty() {
            current_line_spans.push(Span::styled(buf, *style));
        }
    }
}

struct Word {
    fragments: Vec<(String, StyleToken)>,
    separator: StyleToken,
}

fn emit_word(spans: &mut Vec<Span>, word: Vec<(String, StyleToken)>) {
    for (text, style) in word {
        spans.push(Span::styled(text, style));
    }
}

fn collect_words(line: &Line) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    let mut current_word: Vec<(String, StyleToken)> = Vec::new();
    let mut separator = StyleToken::default();

    for span in &line.spans {
        let mut frag = String::new();
        for ch in span.content.chars() {
            if ch.is_whitespace() {
                if !frag.is_empty() {
                    current_word.push((std::mem::take(&mut frag), span.style));
                }
                if !current_word.is_empty() {
                    words.push(Word {
                        fragments: std::mem::take(&mut current_word),
                        separator,
                    });
                }
                separator = span.style;
            } else {
                frag.push(ch);
            }
        }
        if !frag.is_empty() {
            current_word.push((frag, span.style));
        }
    }
    if !current_word.is_empty() {
        words.push(Word {
            fragments: current_word,
            separator,
        });
    }
    words
}

fn calculate_leading_indent(line: &Line) -> usize {
    let mut n = 0;
    for span in &line.spans {
        let spaces = span.content.len() - span.content.trim_start_matches(' ').len();
        n += spaces;
        if spaces < span.content.len() {
            break;
        }
    }
    n
}

/// Wrap a styled Line with hanging indent, preserving all span styles.
#[must_use]
pub fn wrap_styled_line(line: Line, width: usize, continuation_indent: usize) -> Vec<Line> {
    let total_width: usize = line.spans.iter().map(Span::width).sum();
    if total_width <= width {
        return vec![line];
    }

    let mut result_lines = Vec::new();
    let mut current_line_spans: Vec<Span> = Vec::new();
    let mut current_line_width = 0usize;
    let available_width = width.saturating_sub(continuation_indent);
    let leading_indent = calculate_leading_indent(&line);
    let words = collect_words(&line);

    for Word {
        fragments: word,
        separator,
    } in words
    {
        let word_width: usize = word.iter().map(|(text, _)| text.width()).sum();

        if current_line_width == 0 && result_lines.is_empty() {
            if leading_indent > 0 {
                current_line_spans.push(Span::raw(" ".repeat(leading_indent)));
                current_line_width += leading_indent;
            }
            if word_width <= available_width {
                current_line_width += word_width;
                emit_word(&mut current_line_spans, word);
            } else {
                hard_break_styled_word(
                    &word,
                    &mut result_lines,
                    &mut current_line_spans,
                    &mut current_line_width,
                    continuation_indent,
                    available_width,
                    width,
                );
            }
            continue;
        }

        let sep = usize::from(current_line_width > 0);
        if current_line_width + sep + word_width <= available_width {
            if sep == 1 {
                current_line_spans.push(Span::styled(" ", separator));
            }
            current_line_width += sep + word_width;
            emit_word(&mut current_line_spans, word);
        } else if word_width <= available_width {
            result_lines.push(Line::from(std::mem::take(&mut current_line_spans)));
            current_line_spans.push(Span::raw(" ".repeat(continuation_indent)));
            current_line_width = word_width;
            emit_word(&mut current_line_spans, word);
        } else {
            result_lines.push(Line::from(std::mem::take(&mut current_line_spans)));
            current_line_spans.push(Span::raw(" ".repeat(continuation_indent)));
            current_line_width = 0;
            hard_break_styled_word(
                &word,
                &mut result_lines,
                &mut current_line_spans,
                &mut current_line_width,
                continuation_indent,
                available_width,
                available_width,
            );
        }
    }

    if !current_line_spans.is_empty() {
        result_lines.push(Line::from(current_line_spans));
    }

    if result_lines.is_empty() {
        vec![line]
    } else {
        result_lines
    }
}

/// Calculate the number of visual rows rendered by a text buffer wrapped to `width`.
#[must_use]
pub fn rendered_row_count(text: &str, width: usize) -> usize {
    if text.is_empty() || width == 0 {
        return 1;
    }
    let mut count = 0;
    for line in text.split('\n') {
        let w = UnicodeWidthStr::width(line);
        let rows = w.div_ceil(width);
        count += rows.max(1);
    }
    count.max(1)
}

/// Calculate hanging indent (display cells) for hard-wrapping a line.
#[must_use]
pub fn line_hanging_indent(line: &Line) -> usize {
    let mut indent = 0usize;
    for span in &line.spans {
        let text = span.content.as_ref();
        let trimmed = text.trim_start_matches(' ');
        if trimmed.is_empty() {
            indent += span.width();
            continue;
        }
        indent += text.len() - trimmed.len();
        if span.style.fg == Some(ThemeToken::TextSecondary)
            && (trimmed.starts_with("• ")
                || (trimmed.len() >= 3
                    && trimmed.as_bytes()[0].is_ascii_digit()
                    && trimmed.contains(". ")))
            && let Some(pos) = trimmed.find(' ')
        {
            indent += pos + 1;
        }
        break;
    }
    indent
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_styled_line_uses_display_width_for_cjk() {
        let line = Line::from(Span::raw("你好世界".to_string()));
        let wrapped = wrap_styled_line(line, 10, 2);
        assert_eq!(wrapped.len(), 1);
    }

    #[test]
    fn wrap_styled_line_ascii_wraps_when_too_long() {
        let line = Line::from(Span::raw(
            "the quick brown fox jumps over the lazy dog".to_string(),
        ));
        let wrapped = wrap_styled_line(line, 15, 2);
        assert!(wrapped.len() >= 2);
    }

    #[test]
    fn truncate_to_cells_works() {
        assert_eq!(truncate_to_cells("hello world", 8), "hello w…");
        assert_eq!(truncate_to_cells("hello", 10), "hello");
    }
}
