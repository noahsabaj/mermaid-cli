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
#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
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
    let leading_indent: usize = {
        let mut n = 0;
        for span in &line.spans {
            let spaces = span.content.len() - span.content.trim_start_matches(' ').len();
            n += spaces;
            if spaces < span.content.len() {
                break; // this span has non-space content, so leading run ends here
            }
        }
        n
    };

    // Flatten the spans into words: each word is a run of styled fragments plus
    // the style of the whitespace that separated it from the previous word.
    // Whitespace anywhere closes the current word (runs collapse to a single
    // boundary); a span ending mid-word leaves the word open so the next
    // span's text glues on — a style change is NOT a word boundary.
    struct Word {
        fragments: Vec<(String, Style)>,
        /// Style of the whitespace run that preceded this word, taken from the
        /// span that whitespace lived in. Interior gaps of a styled run keep
        /// the run's style; gaps between runs carry the plain prose style.
        separator: Style,
    }
    let mut words: Vec<Word> = Vec::new();
    let mut current_word: Vec<(String, Style)> = Vec::new();
    // Separator in front of the word currently being built. The first word has
    // no preceding gap, so its value is never emitted.
    let mut separator = Style::default();
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
