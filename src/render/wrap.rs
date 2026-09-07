//! Text wrapping: plain and styled, with hard-break fallback for tokens that
//! cannot fit.
//!
//! Lived inside `widgets/chat.rs`, which is why `widgets/question.rs` imported
//! `wrap_styled_line` from a sibling WIDGET. Wrapping is not a chat concern —
//! it is a render-layer primitive that several widgets need — so it sits one
//! level up and that import becomes legitimate.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Hard-break a single over-long token into the plain-text line accumulator,
/// splitting at char boundaries (UTF-8-safe, display-cell aware) so a giant
/// unbroken token (e.g. a 5000-char URL) wraps across lines instead of
/// overflowing the viewport and being clipped (F33).
///
/// Mirrors the accumulation `wrap_text_with_indent` does for normal words:
/// `current_line`/`current_length` carry the in-progress line (its indent
/// already pushed into `current_line`, not counted in `current_length`),
/// finished lines are pushed to `out`, and each new line gets a
/// `continuation_indent`-space hanging indent. `initial_budget` is the content
/// width available on the line the token starts on (the caller's per-line
/// `available_width`); subsequent lines use `width - continuation_indent`.
pub(crate) fn hard_break_plain_token(
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

    // If the current line already holds content, flush it so the token starts
    // fresh on a continuation line; otherwise break onto the current (indent-
    // only) line directly.
    if *current_length > 0 {
        out.push(std::mem::take(current_line));
        current_line.push_str(&" ".repeat(continuation_indent));
        *current_length = 0;
        line_budget = cont_budget;
    }

    for ch in token.chars() {
        let cw = ch.width().unwrap_or(0);
        // Break before this char if it would overflow and the line already
        // holds at least one glyph (so a single too-wide glyph never loops).
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
///
/// `width`, `first_line_indent`, and `continuation_indent` are all measured
/// in **display cells**, not bytes. Word lengths are also measured in cells
/// via `UnicodeWidthStr::width` so CJK / emoji wrap at the visual edge —
/// previously a CJK paragraph would wrap after ~1/3 of the line because
/// `word.len()` (bytes) is roughly 3× `word.width()` (cells) for 3-byte
/// codepoints.
pub(crate) fn wrap_text_with_indent(
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
        // Display-cell widths: indent is ASCII spaces (1 cell each), so
        // start fresh and let words contribute their own cell widths.
        let mut current_length = 0;

        for (word_idx, word) in words.iter().enumerate() {
            let word_width = word.width();

            if word_idx == 0 {
                if word_width <= available_width {
                    // First word fits on the line
                    current_line.push_str(word);
                    current_length = word_width;
                } else {
                    // A single token wider than the whole line (e.g. a long
                    // URL): hard-break it at width boundaries so it wraps
                    // instead of overflowing the viewport and being clipped
                    // (F33).
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
                // Word fits on current line (the +1 accounts for the
                // separator space, which is 1 cell)
                current_line.push(' ');
                current_line.push_str(word);
                current_length += 1 + word_width;
            } else if word_width <= available_width {
                // Word doesn't fit, start a new line
                wrapped_lines.push(current_line);
                current_line = String::with_capacity(width);
                current_line.push_str(&" ".repeat(continuation_indent));
                current_line.push_str(word);
                current_length = word_width;
            } else {
                // Over-long token mid-paragraph: flush the current line, then
                // hard-break the token across continuation lines (F33).
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

        // Add the last line
        if !current_line.trim().is_empty() {
            wrapped_lines.push(current_line);
        }
    }

    wrapped_lines
}

/// Hard-break a single over-long word into the styled line accumulator,
/// splitting at char boundaries (UTF-8-safe, display-cell aware) and keeping
/// each fragment's own style on every produced piece, so a giant unbroken
/// token (e.g. a long URL) wraps across rows instead of overflowing the
/// viewport and being clipped (F33). The styled counterpart of
/// `hard_break_plain_token`. The word arrives as styled fragments (see the
/// flattening pass in `wrap_styled_line`) because a token can change style
/// mid-word (`**bold**suffix`); the break must not flatten that to one style.
///
/// `current_line_spans`/`current_line_width` carry the in-progress row;
/// finished rows are pushed to `result_lines`; each new row opens with a
/// `continuation_indent`-space span. `line_capacity` is the width budget for
/// the row the token starts on (the first row counts its leading indent in
/// `current_line_width`, so its budget is the full `width`); wrapped rows use
/// `continuation_capacity` (the caller's `available_width`, with the indent in
/// a separate span and not counted).
pub(crate) fn hard_break_styled_word(
    fragments: &[(String, Style)],
    result_lines: &mut Vec<Line<'static>>,
    current_line_spans: &mut Vec<Span<'static>>,
    current_line_width: &mut usize,
    continuation_indent: usize,
    continuation_capacity: usize,
    mut line_capacity: usize,
) {
    for (text, style) in fragments {
        let mut buf = String::new();
        for ch in text.chars() {
            let cw = ch.width().unwrap_or(0);
            // Break before this char if it would overflow and the row already
            // holds at least one glyph (so a single too-wide glyph never loops).
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

/// One word of a styled line: a run of styled fragments plus the style of the
/// whitespace that separated it from the previous word.
struct Word {
    fragments: Vec<(String, Style)>,
    /// Style of the whitespace run that preceded this word, taken from the
    /// span that whitespace lived in. Interior gaps of a styled run keep
    /// the run's style; gaps between runs carry the plain prose style.
    separator: Style,
}

/// Cells of leading space across the line's spans, up to the first span that
/// carries non-space content — the "  " continuation gutter a caller prepends.
fn leading_space_cells(spans: &[Span<'static>]) -> usize {
    let mut n = 0;
    for span in spans {
        let spaces = span.content.len() - span.content.trim_start_matches(' ').len();
        n += spaces;
        if spaces < span.content.len() {
            break; // this span has non-space content, so leading run ends here
        }
    }
    n
}

/// Flatten the spans into words: each word is a run of styled fragments plus
/// the style of the whitespace that separated it from the previous word.
/// Whitespace anywhere closes the current word (runs collapse to a single
/// boundary); a span ending mid-word leaves the word open so the next
/// span's text glues on — a style change is NOT a word boundary.
fn flatten_words(spans: &[Span<'static>]) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    let mut current_word: Vec<(String, Style)> = Vec::new();
    // Separator in front of the word currently being built. The first word has
    // no preceding gap, so its value is never emitted.
    let mut separator = Style::default();
    for span in spans {
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
                // This gap belongs to the span it was written in, and becomes
                // the separator in front of the NEXT word — that is what keeps
                // a multi-word code span's background continuous.
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

/// Wrap a styled Line with hanging indent, preserving all span styles.
/// Returns multiple Line objects with proper indentation.
///
/// Wrapping runs over a word stream flattened ACROSS spans: a word is a run of
/// styled fragments, and a word boundary exists only where the source text has
/// whitespace. A span ending mid-word glues onto the next span's text, so
/// `**bold**suffix` stays one token and a `.` right after a link's dimmed URL
/// stays attached (no phantom space at a style boundary).
///
/// A separator space is re-emitted with the style of the span the whitespace
/// CAME FROM, so a gap *inside* a styled run keeps that run's paint while a gap
/// *between* runs stays plain. This is what keeps multi-word inline code
/// (`` `No image data found` ``) one continuous block instead of a row of
/// disconnected per-word boxes, and still leaves the space in front of a link
/// un-underlined (that space belongs to the preceding prose span).
pub(crate) fn wrap_styled_line(
    line: Line<'static>,
    width: usize,
    continuation_indent: usize,
) -> Vec<Line<'static>> {
    // Widths are counted in display cells (via `UnicodeWidthStr`), not
    // bytes. This makes CJK double-width chars and emoji wrap at the
    // correct visual column, and avoids over-wrapping multi-byte ASCII-
    // looking glyphs.
    let total_width: usize = line.spans.iter().map(|s| s.content.width()).sum();

    // If the line fits within width, return as-is
    if total_width <= width {
        return vec![line];
    }

    // Line needs wrapping - extract all text and styles
    let mut result_lines = Vec::new();
    let mut current_line_spans: Vec<Span<'static>> = Vec::new();
    let mut current_line_width = 0usize;
    let available_width = width.saturating_sub(continuation_indent);

    // Preserve the line's existing left margin (the "  " continuation gutter the
    // caller prepends to every non-first message line) on the *first* wrapped
    // segment. The whitespace split below drops leading spaces and the "first
    // word, no indent" rule would then flush the segment to column 0 — that's the
    // recurring bug where a wrapped paragraph escapes the message gutter while its
    // own continuation lines (which get `continuation_indent`) stay aligned. A
    // non-whitespace prefix like "● " is unaffected (it survives the split).
    let leading_indent = leading_space_cells(&line.spans);
    let words = flatten_words(&line.spans);

    fn emit_word(spans: &mut Vec<Span<'static>>, word: Vec<(String, Style)>) {
        for (text, style) in word {
            spans.push(Span::styled(text, style));
        }
    }

    for Word {
        fragments: word,
        separator,
    } in words
    {
        let word_width: usize = word.iter().map(|(text, _)| text.width()).sum();

        if current_line_width == 0 && result_lines.is_empty() {
            // First word of the first line: re-apply the original left margin
            // (dropped by the whitespace split) so the segment keeps the gutter
            // instead of flushing to column 0.
            if leading_indent > 0 {
                current_line_spans.push(Span::raw(" ".repeat(leading_indent)));
                current_line_width += leading_indent;
            }
            if word_width <= available_width {
                current_line_width += word_width;
                emit_word(&mut current_line_spans, word);
            } else {
                // A single token wider than the line (e.g. a long URL):
                // hard-break it at width boundaries so it wraps instead of
                // being clipped by the viewport (F33). The first row may use
                // the full `width` (its indent is already counted above);
                // continuation rows fall back to `available_width`.
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

        // Separator space before this word — only when the row already holds
        // content, and painted with the style of the span the gap came from
        // (see the flattening pass): interior gaps of a code span keep its
        // background, gaps between runs stay plain. A gap that lands on a wrap
        // point is dropped entirely, so no row ends in a highlighted space.
        let sep = usize::from(current_line_width > 0);
        if current_line_width + sep + word_width <= available_width {
            // Word fits on current line
            if sep == 1 {
                current_line_spans.push(Span::styled(" ", separator));
            }
            current_line_width += sep + word_width;
            emit_word(&mut current_line_spans, word);
        } else if word_width <= available_width {
            // Word doesn't fit - finish current line and start new one
            result_lines.push(Line::from(std::mem::take(&mut current_line_spans)));
            current_line_spans.push(Span::raw(" ".repeat(continuation_indent)));
            current_line_width = word_width;
            emit_word(&mut current_line_spans, word);
        } else {
            // Over-long token mid-line: finish the current line, then
            // hard-break the token across continuation rows (F33), keeping
            // each fragment's style on every produced piece.
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

    // Add the last line if it has content
    if !current_line_spans.is_empty() {
        result_lines.push(Line::from(current_line_spans));
    }

    if result_lines.is_empty() {
        vec![line]
    } else {
        result_lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CJK characters are 3 bytes but 2 display cells each. The
    /// byte-length version of `wrap_styled_line` would incorrectly
    /// over-wrap such input. This test asserts the display-width
    /// version keeps CJK-only input on a single line when the display
    /// width fits, even when the byte length exceeds the width.
    #[test]
    fn wrap_styled_line_uses_display_width_for_cjk() {
        // "你好世界" is 4 CJK chars × 3 bytes = 12 bytes, × 2 display cells = 8 cells.
        // Target width of 10: byte-length would see 12 > 10 and wrap;
        // display-width sees 8 <= 10 and keeps it on one line.
        let line = Line::from(Span::raw("你好世界".to_string()));
        let wrapped = wrap_styled_line(line, 10, 2);
        assert_eq!(
            wrapped.len(),
            1,
            "CJK input fitting in display-width should NOT be wrapped; got {} lines",
            wrapped.len()
        );
    }

    /// Sanity: ASCII wrapping still works and produces >= 2 lines when
    /// the input exceeds the width.
    #[test]
    fn wrap_styled_line_ascii_wraps_when_too_long() {
        let line = Line::from(Span::raw(
            "the quick brown fox jumps over the lazy dog".to_string(),
        ));
        let wrapped = wrap_styled_line(line, 15, 2);
        assert!(
            wrapped.len() >= 2,
            "long ASCII input should wrap to multiple lines; got {}",
            wrapped.len()
        );
    }

    fn first_segment_text(wrapped: &[Line<'static>]) -> String {
        wrapped[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    /// Regression (recurring "paragraph escapes the gutter" bug): a non-first
    /// message line carries a 2-space gutter prefix; when it wraps, the first
    /// segment must keep that gutter, not flush to column 0. `split_whitespace`
    /// used to drop the leading spaces and the "first word, no indent" rule
    /// flushed the segment left.
    #[test]
    fn wrap_styled_line_keeps_gutter_on_wrapped_paragraph() {
        let line = Line::from(vec![
            Span::raw("  "), // the continuation gutter chat.rs prepends
            Span::raw(
                "No source files, no config, no docs, no build system and more words to wrap"
                    .to_string(),
            ),
        ]);
        let wrapped = wrap_styled_line(line, 30, 2);
        assert!(wrapped.len() >= 2, "should wrap");
        let first = first_segment_text(&wrapped);
        assert!(
            first.starts_with("  ") && first.trim_start().starts_with("No source"),
            "first wrapped segment must keep the 2-space gutter; got {first:?}"
        );
    }

    /// Regression: a multi-word inline code span used to render as one box per
    /// word. The wrapper re-emitted every separator space unstyled, punching
    /// plain gaps through the code background — every wrapped answer containing
    /// `` `a phrase like this` `` came out visually shredded. Gaps *inside* a
    /// styled run now keep that run's style; the gap *before* it stays plain.
    #[test]
    fn wrap_styled_line_keeps_inline_code_background_across_its_spaces() {
        let code = Style::default().bg(ratatui::style::Color::Rgb(40, 40, 40));
        let line = Line::from(vec![
            Span::raw("read_image_bytes bails with ".to_string()),
            Span::styled("No image data found in clipboard".to_string(), code),
            Span::raw(" and the effect routes it onward".to_string()),
        ]);
        let wrapped = wrap_styled_line(line, 40, 2);
        assert!(wrapped.len() >= 2, "should wrap");

        // Walk the produced spans in order: every space BETWEEN two code-styled
        // spans must itself be code-styled; the space before the run must not.
        let spans: Vec<_> = wrapped.iter().flat_map(|l| l.spans.iter()).collect();
        let interior_gaps = spans
            .windows(3)
            .filter(|w| {
                w[1].content.as_ref() == " " && w[0].style.bg.is_some() && w[2].style.bg.is_some()
            })
            .count();
        assert!(
            interior_gaps >= 3,
            "the 5-word code span should keep its background on interior gaps; got \
             {interior_gaps} in {:?}",
            spans
                .iter()
                .map(|s| (s.content.as_ref(), s.style.bg))
                .collect::<Vec<_>>()
        );
        assert!(
            spans.windows(2).all(|w| {
                !(w[0].content.as_ref() == " "
                    && w[0].style.bg.is_some()
                    && w[1].style.bg.is_none())
            }),
            "no highlighted space may leak onto the plain prose that follows"
        );
    }

    /// End-to-end: a wrapped list item keeps the bullet on the first segment and
    /// hangs its continuation lines under the item text (col 6 = 2 gutter + 2
    /// nesting indent + 2 marker), instead of snapping back to the message gutter.
    /// Exercises the same span shape chat.rs builds, with the continuation indent
    /// chat.rs derives via `markdown::line_hanging_indent` (4) + the gutter (2).
    #[test]
    fn wrap_styled_line_hangs_list_continuation_under_marker() {
        let line = Line::from(vec![
            Span::raw("  "), // message gutter (chat.rs)
            Span::raw("  "), // list nesting indent (markdown)
            Span::raw("• "), // marker (markdown)
            Span::raw("alpha beta gamma delta epsilon zeta eta theta iota".to_string()),
        ]);
        let wrapped = wrap_styled_line(line, 24, 6);
        assert!(wrapped.len() >= 2, "should wrap");
        assert!(
            first_segment_text(&wrapped).starts_with("    • "),
            "first segment keeps gutter + nesting + marker"
        );
        for cont in &wrapped[1..] {
            let t: String = cont.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                t.starts_with("      ") && t.chars().nth(6).is_some_and(|c| c != ' '),
                "continuation hangs under the item text at col 6; got {t:?}"
            );
        }
    }

    /// The fix preserves whitespace margins only — the message bullet "● " must
    /// still sit at column 0 on the first line.
    #[test]
    fn wrap_styled_line_keeps_bullet_at_column_zero() {
        let line = Line::from(vec![
            Span::raw("● "),
            Span::raw(
                "a fairly long first line of a message that definitely needs to wrap".to_string(),
            ),
        ]);
        let wrapped = wrap_styled_line(line, 25, 2);
        assert!(wrapped.len() >= 2, "should wrap");
        assert!(
            first_segment_text(&wrapped).starts_with('●'),
            "bullet must stay at column 0"
        );
    }

    /// Counterpart to `wrap_styled_line_uses_display_width_for_cjk` for
    /// the plain-string wrapper used by user messages and thinking blocks.
    /// The byte-based version would wrap a 4-CJK paragraph after the second
    /// char (12 bytes > 10) even though it fits in 8 cells. Display-width
    /// version keeps it on one line.
    #[test]
    fn wrap_text_with_indent_uses_display_width_for_cjk() {
        // "你好世界" = 4 chars, 12 bytes, 8 display cells. Width 12 cells
        // with 0 indent: should fit on one line.
        let wrapped = wrap_text_with_indent("你好世界", 12, 0, 0);
        assert_eq!(
            wrapped.len(),
            1,
            "CJK paragraph fitting in display width should not wrap; got {} lines: {:?}",
            wrapped.len(),
            wrapped
        );
        assert_eq!(wrapped[0].trim_start(), "你好世界");
    }

    /// Mixed content: CJK + ASCII should still wrap correctly when the
    /// total exceeds available cells.
    #[test]
    fn wrap_text_with_indent_wraps_cjk_at_visual_edge() {
        // "你好 world 世界" = 2 + 1 + 5 + 1 + 2 = 11 cells without spaces,
        // with separators: 2 + 1 + 5 + 1 + 4 = 13 cells. Width 8 cells should
        // produce ≥ 2 lines.
        let wrapped = wrap_text_with_indent("你好 world 世界", 8, 0, 0);
        assert!(
            wrapped.len() >= 2,
            "mixed CJK+ASCII exceeding width should wrap; got {} lines: {:?}",
            wrapped.len(),
            wrapped
        );
    }

    #[test]
    fn wrap_text_with_indent_hard_breaks_overlong_token() {
        // F33: a single unbroken token far wider than the viewport must
        // hard-break at width boundaries instead of overflowing and being
        // clipped. No internal spaces, so word-wrapping alone can't split it.
        let token = "x".repeat(100);
        let width = 20;
        let wrapped = wrap_text_with_indent(&token, width, 2, 2);
        assert!(
            wrapped.len() >= 5,
            "a 100-cell token at width 20 must span many rows; got {}",
            wrapped.len()
        );
        for line in &wrapped {
            assert!(
                line.chars().count() <= width,
                "no wrapped row may exceed the width; got {:?} ({} cells)",
                line,
                line.chars().count()
            );
        }
        // Stripping each row's hanging indent reconstructs the token intact.
        let joined: String = wrapped.iter().map(|l| l.trim_start()).collect();
        assert_eq!(
            joined, token,
            "hard-break must preserve the token's content"
        );
    }

    #[test]
    fn wrap_styled_line_hard_breaks_overlong_token() {
        // F33 (styled path): the same hard-break, preserving each piece's style.
        let token = "y".repeat(90);
        let style = Style::new().fg(ratatui::style::Color::Red);
        let line = Line::from(vec![Span::raw("  "), Span::styled(token.clone(), style)]);
        let width = 24;
        let wrapped = wrap_styled_line(line, width, 2);
        assert!(
            wrapped.len() >= 4,
            "must hard-break across rows; got {}",
            wrapped.len()
        );

        let mut reconstructed = String::new();
        for l in &wrapped {
            let row_cells: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(
                row_cells <= width,
                "row exceeds width: {row_cells} > {width}"
            );
            for s in &l.spans {
                // Skip indent/gutter spans (whitespace only); every content
                // piece must keep the original red foreground.
                if s.content.trim().is_empty() {
                    continue;
                }
                assert_eq!(
                    s.style.fg,
                    Some(ratatui::style::Color::Red),
                    "hard-break must preserve the span style"
                );
                reconstructed.push_str(s.content.as_ref());
            }
        }
        assert_eq!(reconstructed, token, "hard-break must preserve the token");
    }

    /// The separator space re-inserted between words must be unstyled: when a
    /// wrapped line contains an underlined link span, the gap before the link
    /// used to inherit the underline (visibly underlined space in the TUI).
    #[test]
    fn wrap_styled_line_separator_before_styled_span_is_unstyled() {
        let underlined = Style::new().add_modifier(ratatui::style::Modifier::UNDERLINED);
        let line = Line::from(vec![
            Span::raw("  "),
            Span::raw("some filler words long enough to force a wrap here "),
            Span::styled("underlined-link-text", underlined),
            Span::raw(" and a bit more trailing filler after the link"),
        ]);
        let wrapped = wrap_styled_line(line, 30, 2);
        assert!(wrapped.len() >= 2, "fixture must actually wrap");
        for l in &wrapped {
            for s in &l.spans {
                if s.content.chars().all(|c| c == ' ') {
                    assert_eq!(
                        s.style,
                        Style::default(),
                        "whitespace span {:?} must be unstyled",
                        s.content
                    );
                }
            }
        }
    }

    /// A span boundary WITHOUT source whitespace is not a word boundary: the
    /// dimmed "(url)" suffix a markdown link gets, followed by a bare "." text
    /// span, must stay "(url)." — not gain a phantom space ("(url) .").
    #[test]
    fn wrap_styled_line_no_phantom_space_at_span_boundary() {
        let dim = Style::new().fg(ratatui::style::Color::DarkGray);
        let line = Line::from(vec![
            Span::raw("  "),
            Span::raw("filler text that pushes the line well past the width limit "),
            Span::styled("(https://example.com)".to_string(), dim),
            Span::raw("."),
        ]);
        let wrapped = wrap_styled_line(line, 30, 2);
        assert!(wrapped.len() >= 2, "fixture must actually wrap");
        let text: String = wrapped
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            text.contains("(https://example.com)."),
            "period must stay glued to the URL suffix; got {text:?}"
        );
        assert!(
            !text.contains("(https://example.com) ."),
            "no phantom space before the period; got {text:?}"
        );
    }

    /// A style change mid-word ("**bold**suffix") is not a word boundary: the
    /// two fragments must land on the same row as one token, each keeping its
    /// own style.
    #[test]
    fn wrap_styled_line_keeps_mid_word_style_change_glued() {
        let bold = Style::new().add_modifier(ratatui::style::Modifier::BOLD);
        let line = Line::from(vec![
            Span::raw("  "),
            Span::raw("leading filler words to force wrapping "),
            Span::styled("bold", bold),
            Span::raw("suffix"),
            Span::raw(" trailing filler words to force more wrapping"),
        ]);
        let wrapped = wrap_styled_line(line, 30, 2);
        assert!(wrapped.len() >= 2, "fixture must actually wrap");
        let rows: Vec<String> = wrapped
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(
            rows.iter().filter(|r| r.contains("boldsuffix")).count(),
            1,
            "glued token must land whole on exactly one row; rows: {rows:?}"
        );
        for l in &wrapped {
            for s in &l.spans {
                if s.content.as_ref() == "bold" {
                    assert_eq!(s.style, bold, "bold fragment keeps its modifier");
                }
                if s.content.as_ref() == "suffix" {
                    assert_eq!(s.style, Style::default(), "suffix fragment stays plain");
                }
            }
        }
    }

    /// An over-long glued token made of differently styled fragments must
    /// hard-break across rows with each fragment's style preserved and no
    /// content lost — it enters the break path as ONE token, not two words.
    #[test]
    fn wrap_styled_line_hard_breaks_multi_fragment_token_preserving_styles() {
        let red = Style::new().fg(ratatui::style::Color::Red);
        let blue = Style::new().fg(ratatui::style::Color::Blue);
        let line = Line::from(vec![
            Span::raw("  "),
            Span::styled("a".repeat(40), red),
            Span::styled("b".repeat(40), blue),
        ]);
        let width = 24;
        let wrapped = wrap_styled_line(line, width, 2);
        assert!(
            wrapped.len() >= 4,
            "80-cell token at width 24 must span >= 4 rows; got {}",
            wrapped.len()
        );
        let mut reconstructed = String::new();
        for l in &wrapped {
            let row_cells: usize = l.spans.iter().map(|s| s.content.width()).sum();
            assert!(
                row_cells <= width,
                "row exceeds width: {row_cells} > {width}"
            );
            for s in &l.spans {
                if s.content.trim().is_empty() {
                    continue;
                }
                let expected = if s.content.contains('a') { red } else { blue };
                assert!(
                    !(s.content.contains('a') && s.content.contains('b')),
                    "fragments must not merge across the style boundary"
                );
                assert_eq!(s.style, expected, "fragment style preserved across break");
                reconstructed.push_str(s.content.as_ref());
            }
        }
        assert_eq!(
            reconstructed,
            format!("{}{}", "a".repeat(40), "b".repeat(40)),
            "hard-break must preserve the whole glued token"
        );
    }

    /// A whitespace-only span between two text spans still separates words —
    /// gluing only happens where the source truly has no whitespace.
    #[test]
    fn wrap_styled_line_whitespace_only_span_is_word_boundary() {
        let line = Line::from(vec![
            Span::raw("  "),
            Span::raw("filler words that push this line past the wrap width "),
            Span::raw("foo"),
            Span::raw(" "),
            Span::raw("bar"),
        ]);
        let wrapped = wrap_styled_line(line, 30, 2);
        assert!(wrapped.len() >= 2, "fixture must actually wrap");
        let text: String = wrapped
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("foo bar") || text.contains("foo\n  bar"),
            "whitespace-only span must keep the words apart; got {text:?}"
        );
        assert!(
            !text.contains("foobar"),
            "words must not glue; got {text:?}"
        );
    }
}
