use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::render::theme::Theme;

/// A parsed markdown line plus whether it is **preformatted** — i.e. must NOT be
/// word-wrapped by the chat renderer (which collapses runs of whitespace). Code
/// blocks and tables are preformatted: their exact spacing carries meaning
/// (indentation, column alignment). Everything else word-wraps normally.
#[derive(Debug, Clone)]
pub struct MarkdownLine {
    pub line: Line<'static>,
    pub preformatted: bool,
}

#[derive(Debug, Clone)]
struct ListState {
    next_number: Option<u64>,
    /// Leading whitespace that aligns a continuation block (a 2nd+ paragraph in
    /// a loose list item) under the current item's text — set when the item's
    /// marker is emitted.
    cont_indent: String,
}

/// Style the list markers ("• ", "1. ") are emitted with. Shared with
/// [`line_hanging_indent`] so the indent logic recognizes a marker span without
/// the two definitions drifting apart.
fn list_marker_style(theme: &Theme) -> Style {
    Style::new().fg(theme.colors.text_secondary.to_color())
}

/// Hanging indent (display cells) for hard-wrapping `line`: the column where the
/// line's content begins, so a wrapped list item's continuation lines align
/// under its text (after the marker) instead of snapping back to the flat
/// message gutter. Counts leading whitespace plus a leading list marker; returns
/// 0 for ordinary paragraphs and headings.
pub fn line_hanging_indent(line: &Line, theme: &Theme) -> usize {
    let marker = list_marker_style(theme);
    let mut indent = 0usize;
    for span in &line.spans {
        let text = span.content.as_ref();
        let trimmed = text.trim_start_matches(' ');
        if trimmed.is_empty() {
            indent += text.width(); // a blank span is part of the leading indent
            continue;
        }
        // First span with real content: count its own leading spaces, and include
        // the marker glyph itself when this span is the list marker.
        indent += text.width() - trimmed.width();
        if span.style == marker {
            indent += trimmed.width();
        }
        break;
    }
    indent
}

/// Parse markdown into theme-styled lines, each flagged [`MarkdownLine::preformatted`]
/// when it must not be word-wrapped — code blocks and tables, whose exact spacing
/// carries meaning (indentation, column alignment). `width` is the available
/// content width in display cells; tables are sized and wrapped to fit it.
///
/// Code-block lines also keep the theme's `code_background` on their base style
/// (the gray panel look). Inline code carries the background on the *span*, not
/// the line, so prose that merely contains `code` still word-wraps normally.
#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
pub fn parse_markdown(input: &str, theme: &Theme, width: usize) -> Vec<MarkdownLine> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);

    // Resolve the theme palette once.
    let c = &theme.colors;
    let code_bg = c.code_background.to_color();
    let code_fg = c.code_foreground.to_color();
    let heading1 = Style::new().fg(c.header.to_color()).bold();
    let heading2 = Style::new().fg(c.info.to_color()).bold();
    let heading3 = Style::new().fg(c.success.to_color()).bold();
    let heading_other = Style::new().fg(c.warning.to_color()).bold();
    let link_style = Style::new()
        .fg(c.info.to_color())
        .add_modifier(Modifier::UNDERLINED);
    let marker_style = list_marker_style(theme);
    let rule_style = Style::new().fg(c.text_disabled.to_color());
    let quote_bar_style = Style::new().fg(c.text_disabled.to_color());
    let quote_text_style = Style::new()
        .fg(c.text_secondary.to_color())
        .add_modifier(Modifier::ITALIC);

    let parser = Parser::new_ext(input, options);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_line_spans: Vec<Span<'static>> = Vec::new();
    let mut style_stack = vec![Style::default()];
    let mut in_code_block = false;
    let mut code_block_content = String::new();
    let mut code_block_lang = String::new();
    let mut current_link_url: Option<String> = None;
    let mut list_stack: Vec<ListState> = Vec::new();

    // Table state
    let mut in_table = false;
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut current_row: Vec<String> = Vec::new();
    let mut current_cell = String::new();
    let mut table_header_len: usize = 0;
    // Indices into `lines` produced by `render_table` — these are preformatted
    // (column-aligned) and must not be word-wrapped by the chat renderer.
    let mut table_line_indices: std::collections::HashSet<usize> = std::collections::HashSet::new();

    for event in parser {
        match event {
            Event::Start(tag) => {
                let new_style = match tag {
                    Tag::Heading { level, .. } => {
                        if !current_line_spans.is_empty() {
                            lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                        }
                        // Blank line before heading (except the first thing).
                        if !lines.is_empty() {
                            lines.push(Line::from(""));
                        }
                        match level {
                            HeadingLevel::H1 => heading1,
                            HeadingLevel::H2 => heading2,
                            HeadingLevel::H3 => heading3,
                            _ => heading_other,
                        }
                    },
                    Tag::Emphasis => style_stack.last().copied().unwrap_or_default().italic(),
                    Tag::Strong => style_stack.last().copied().unwrap_or_default().bold(),
                    Tag::Strikethrough => style_stack
                        .last()
                        .copied()
                        .unwrap_or_default()
                        .crossed_out(),
                    Tag::CodeBlock(kind) => {
                        in_code_block = true;
                        code_block_content.clear();
                        if !current_line_spans.is_empty() {
                            lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                        }
                        code_block_lang = match kind {
                            CodeBlockKind::Fenced(lang) => lang.to_string(),
                            CodeBlockKind::Indented => String::new(),
                        };
                        if !code_block_lang.is_empty() {
                            lines.push(Line::from(Span::styled(
                                code_block_lang.clone(),
                                Style::new()
                                    .fg(c.text_disabled.to_color())
                                    .add_modifier(Modifier::ITALIC),
                            )));
                        }
                        Style::default().fg(code_fg)
                    },
                    Tag::List(start) => {
                        list_stack.push(ListState {
                            next_number: start,
                            cont_indent: String::new(),
                        });
                        if !current_line_spans.is_empty() {
                            lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                        }
                        style_stack.last().copied().unwrap_or_default()
                    },
                    Tag::Item => {
                        let indent = "  ".repeat(list_stack.len());
                        let marker = if let Some(state) = list_stack.last_mut() {
                            if let Some(current) = state.next_number {
                                state.next_number = Some(current + 1);
                                format!("{}. ", current)
                            } else {
                                "• ".to_string()
                            }
                        } else {
                            "• ".to_string()
                        };
                        // Body paragraphs of this item hang-indent to align under
                        // the text that follows the marker.
                        let cont_indent =
                            format!("{}{}", indent, " ".repeat(marker.as_str().width()));
                        if let Some(state) = list_stack.last_mut() {
                            state.cont_indent = cont_indent;
                        }
                        current_line_spans.push(Span::raw(indent));
                        current_line_spans.push(Span::styled(marker, marker_style));
                        style_stack.last().copied().unwrap_or_default()
                    },
                    Tag::Paragraph => {
                        // First paragraph of an item still carries the marker
                        // spans (non-empty); a continuation paragraph starts
                        // empty, so re-indent it to align under the item text.
                        if current_line_spans.is_empty()
                            && let Some(state) = list_stack.last()
                            && !state.cont_indent.is_empty()
                        {
                            current_line_spans.push(Span::raw(state.cont_indent.clone()));
                        }
                        style_stack.last().copied().unwrap_or_default()
                    },
                    Tag::Table(_alignments) => {
                        in_table = true;
                        table_rows.clear();
                        table_header_len = 0;
                        if !current_line_spans.is_empty() {
                            lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                        }
                        style_stack.last().copied().unwrap_or_default()
                    },
                    Tag::TableHead | Tag::TableRow => {
                        current_row.clear();
                        style_stack.last().copied().unwrap_or_default()
                    },
                    Tag::TableCell => {
                        current_cell.clear();
                        style_stack.last().copied().unwrap_or_default()
                    },
                    Tag::Link { dest_url, .. } => {
                        // Render the link text underlined in the accent color;
                        // the destination URL is appended dimmed on the End tag
                        // (terminals can't follow it without OSC-8).
                        current_link_url = Some(dest_url.to_string());
                        link_style
                    },
                    Tag::BlockQuote(_) => {
                        if !current_line_spans.is_empty() {
                            lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                        }
                        current_line_spans.push(Span::styled("│ ", quote_bar_style));
                        quote_text_style
                    },
                    _ => style_stack.last().copied().unwrap_or_default(),
                };
                style_stack.push(new_style);
            },
            Event::End(tag) => {
                style_stack.pop();
                match tag {
                    // Every block-level close flushes the pending inline spans
                    // as one finished line.
                    TagEnd::Heading(_)
                    | TagEnd::Paragraph
                    | TagEnd::Item
                    | TagEnd::BlockQuote(_)
                        if !current_line_spans.is_empty() =>
                    {
                        lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                    },
                    TagEnd::CodeBlock => {
                        in_code_block = false;
                        let prefixes = line_comment_prefixes(&code_block_lang);
                        let base = Style::default().fg(code_fg).bg(code_bg);
                        for line_text in code_block_content.lines() {
                            let spans = highlight_code_line(line_text, prefixes, theme);
                            // Mark the LINE base style with the code bg so the
                            // chat renderer treats it as pre-formatted.
                            lines.push(Line::from(spans).style(base));
                        }
                        code_block_content.clear();
                        code_block_lang.clear();
                    },
                    TagEnd::List(_) => {
                        let _ = list_stack.pop();
                        if list_stack.is_empty() {
                            lines.push(Line::from(""));
                        }
                    },
                    TagEnd::TableCell => {
                        current_row.push(std::mem::take(&mut current_cell));
                    },
                    TagEnd::TableHead => {
                        table_header_len = current_row.len();
                        table_rows.push(std::mem::take(&mut current_row));
                    },
                    TagEnd::TableRow => {
                        table_rows.push(std::mem::take(&mut current_row));
                    },
                    TagEnd::Table => {
                        in_table = false;
                        let from = lines.len();
                        render_table(&mut lines, &table_rows, table_header_len, theme, width);
                        table_line_indices.extend(from..lines.len());
                        table_rows.clear();
                    },
                    TagEnd::Link => {
                        // Append the destination as dimmed " (url)" unless it's
                        // identical to the visible text (autolinks) or empty.
                        if let Some(url) = current_link_url.take() {
                            let text: String = current_line_spans
                                .iter()
                                .map(|s| s.content.as_ref())
                                .collect();
                            if !url.is_empty() && !text.ends_with(&url) {
                                current_line_spans.push(Span::styled(
                                    format!(" ({})", url),
                                    Style::new().fg(c.text_disabled.to_color()),
                                ));
                            }
                        }
                    },
                    _ => {},
                }
            },
            Event::Text(text) => {
                if in_code_block {
                    code_block_content.push_str(&text);
                } else if in_table {
                    current_cell.push_str(&text);
                } else {
                    let style = style_stack.last().copied().unwrap_or_default();
                    current_line_spans.push(Span::styled(text.to_string(), style));
                }
            },
            Event::Code(code) => {
                if in_table {
                    current_cell.push_str(&code);
                } else {
                    // Inline code: tight (no padding spaces), code colors. The
                    // background lives on the SPAN only — prose lines with
                    // inline code still word-wrap normally.
                    let style = Style::default().fg(code_fg).bg(code_bg);
                    current_line_spans.push(Span::styled(code.to_string(), style));
                }
            },
            Event::Rule => {
                if !current_line_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                }
                lines.push(Line::from(Span::styled("─".repeat(40), rule_style)));
            },
            Event::SoftBreak | Event::HardBreak if !current_line_spans.is_empty() => {
                lines.push(Line::from(std::mem::take(&mut current_line_spans)));
            },
            _ => {},
        }
    }

    if !current_line_spans.is_empty() {
        lines.push(Line::from(current_line_spans));
    }

    // A line is preformatted (no word-wrap) if it's a code-block line (tagged with
    // the code background on its base style) or a table line (column-aligned).
    lines
        .into_iter()
        .enumerate()
        .map(|(i, line)| MarkdownLine {
            preformatted: line.style.bg == Some(code_bg) || table_line_indices.contains(&i),
            line,
        })
        .collect()
}

/// Render the accumulated table rows into aligned, themed lines that fit `width`
/// display cells. Column widths come from content (CJK-safe, min 3); if the
/// natural table is wider than `width`, the widest columns are shrunk and long
/// cells are word-wrapped within their column — so nothing is lost and no row
/// overflows the viewport.
fn render_table(
    lines: &mut Vec<Line<'static>>,
    table_rows: &[Vec<String>],
    table_header_len: usize,
    theme: &Theme,
    width: usize,
) {
    let c = &theme.colors;
    let num_cols = table_rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if num_cols == 0 {
        return;
    }

    // Natural column widths in DISPLAY CELLS (CJK-safe), min 3.
    let mut col_widths = vec![0usize; num_cols];
    for row in table_rows {
        for (i, cell) in row.iter().enumerate() {
            if i < num_cols {
                col_widths[i] = col_widths[i].max(cell.width());
            }
        }
    }
    for w in &mut col_widths {
        *w = (*w).max(3);
    }

    // Borders/padding cost: leading "| " (2) + " | " (3) per column. If the table
    // is wider than the viewport, shrink the widest columns until it fits; cell
    // text is then wrapped within the budgeted width. Columns shrink all the way
    // to a 1-cell minimum on a narrow terminal (no `num_cols * 3` budget floor),
    // so a many-column table fits instead of overflowing and clipping at the edge.
    // (In the extreme where the per-column border overhead alone exceeds the
    // width — more columns than the terminal has cells — nothing fits in-budget
    // and the terminal clips the row; that's unavoidable without dropping columns.)
    let overhead = 2 + 3 * num_cols;
    if col_widths.iter().sum::<usize>() + overhead > width {
        let budget = width.saturating_sub(overhead);
        let mut total: usize = col_widths.iter().sum();
        while total > budget {
            let widest = (0..num_cols)
                .filter(|&i| col_widths[i] > 1)
                .max_by_key(|&i| col_widths[i]);
            match widest {
                Some(i) => {
                    col_widths[i] -= 1;
                    total -= 1;
                },
                None => break, // every column already at the 1-cell floor
            }
        }
    }

    let border_style = Style::default().fg(c.text_disabled.to_color());
    let header_style = Style::default().fg(c.header.to_color()).bold();
    let cell_style = Style::default().fg(c.text_primary.to_color());

    for (row_idx, row) in table_rows.iter().enumerate() {
        let style = if row_idx == 0 && table_header_len > 0 {
            header_style
        } else {
            cell_style
        };
        // Wrap each cell to its column width; the row is as tall as its tallest cell.
        let wrapped: Vec<Vec<String>> = (0..num_cols)
            .map(|ci| {
                wrap_cell(
                    row.get(ci).map(String::as_str).unwrap_or(""),
                    col_widths[ci],
                )
            })
            .collect();
        let row_height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);

        for li in 0..row_height {
            let mut spans = vec![Span::styled("| ", border_style)];
            for ci in 0..num_cols {
                let w = col_widths[ci];
                let cell_line = wrapped[ci].get(li).map(String::as_str).unwrap_or("");
                let padding = w.saturating_sub(cell_line.width());
                let padded = format!("{}{}", cell_line, " ".repeat(padding));
                spans.push(Span::styled(padded, style));
                spans.push(Span::styled(" | ", border_style));
            }
            lines.push(Line::from(spans));
        }

        if row_idx == 0 && table_header_len > 0 {
            let mut sep_spans = vec![Span::styled("|-", border_style)];
            for (col, &w) in col_widths.iter().enumerate() {
                sep_spans.push(Span::styled("-".repeat(w), border_style));
                // A data row ends `" | "` — pipe then a SPACE, which is
                // invisible. Ending the separator `"-|-"` put a dash where that
                // space is, so every table hung one stray dash past its right
                // edge. The last column closes on the pipe instead.
                let closer = if col + 1 == num_cols { "-|" } else { "-|-" };
                sep_spans.push(Span::styled(closer, border_style));
            }
            lines.push(Line::from(sep_spans));
        }
    }

    lines.push(Line::from(""));
}

/// Word-wrap `text` to `width` display cells, hard-breaking any word longer than
/// the column. Always returns at least one (possibly empty) line.
fn wrap_cell(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for word in text.split_whitespace() {
        let ww = word.width();
        if ww > width {
            // A word too wide for the column: flush the current line, then
            // hard-break the word; the final chunk stays open so the next word
            // can continue after it.
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            let chunks = chunk_by_width(word, width);
            let n = chunks.len();
            for (k, chunk) in chunks.into_iter().enumerate() {
                if k + 1 < n {
                    lines.push(chunk);
                } else {
                    cur_w = chunk.width();
                    cur = chunk;
                }
            }
            continue;
        }
        let sep = usize::from(!cur.is_empty());
        if cur_w + sep + ww > width {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
            cur_w = ww;
        } else {
            if sep == 1 {
                cur.push(' ');
            }
            cur.push_str(word);
            cur_w += sep + ww;
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Split `s` into chunks each at most `width` display cells, never splitting a
/// character. Used to hard-break a word longer than its column.
fn chunk_by_width(s: &str, width: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if cur_w + cw > width && !cur.is_empty() {
            chunks.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push(ch);
        cur_w += cw;
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

/// Line-comment prefix(es) for a fenced-code language hint. Falls back to a
/// permissive set so unknown languages still get comment coloring.
fn line_comment_prefixes(lang: &str) -> &'static [&'static str] {
    match lang.trim().to_ascii_lowercase().as_str() {
        "rust" | "rs" | "c" | "cpp" | "c++" | "h" | "hpp" | "java" | "js" | "javascript" | "ts"
        | "typescript" | "tsx" | "jsx" | "go" | "golang" | "swift" | "kotlin" | "kt" | "scala"
        | "cs" | "csharp" | "php" | "dart" | "zig" | "rust,no_run" => &["//"],
        "python" | "py" | "ruby" | "rb" | "sh" | "bash" | "zsh" | "shell" | "console" | "yaml"
        | "yml" | "toml" | "ini" | "perl" | "pl" | "r" | "elixir" | "ex" | "makefile"
        | "dockerfile" | "nix" => &["#"],
        "sql" | "lua" | "haskell" | "hs" | "ada" => &["--"],
        "lisp" | "clojure" | "clj" | "scheme" | "el" => &[";"],
        _ => &["//", "#"],
    }
}

/// Cross-language keyword set for the lightweight in-house highlighter.
fn is_keyword(w: &str) -> bool {
    matches!(
        w,
        "fn" | "let"
            | "const"
            | "mut"
            | "pub"
            | "struct"
            | "enum"
            | "impl"
            | "trait"
            | "use"
            | "mod"
            | "match"
            | "if"
            | "else"
            | "for"
            | "while"
            | "loop"
            | "return"
            | "break"
            | "continue"
            | "async"
            | "await"
            | "move"
            | "ref"
            | "where"
            | "type"
            | "dyn"
            | "as"
            | "in"
            | "static"
            | "unsafe"
            | "extern"
            | "crate"
            | "self"
            | "Self"
            | "super"
            | "function"
            | "var"
            | "def"
            | "class"
            | "import"
            | "from"
            | "export"
            | "default"
            | "public"
            | "private"
            | "protected"
            | "void"
            | "int"
            | "long"
            | "float"
            | "double"
            | "bool"
            | "boolean"
            | "char"
            | "string"
            | "true"
            | "false"
            | "null"
            | "nil"
            | "None"
            | "True"
            | "False"
            | "this"
            | "new"
            | "try"
            | "catch"
            | "finally"
            | "throw"
            | "throws"
            | "package"
            | "interface"
            | "extends"
            | "implements"
            | "do"
            | "then"
            | "elif"
            | "lambda"
            | "yield"
            | "with"
            | "and"
            | "or"
            | "not"
            | "is"
            | "end"
            | "begin"
            | "val"
            | "func"
            | "defer"
            | "select"
            | "chan"
            | "range"
            | "switch"
            | "case"
    )
}

/// Tokenize one code line into styled spans (all sharing the code background)
/// using a small, language-agnostic lexer: line comments, quoted strings, and
/// a cross-language keyword set. Everything else is the default code color.
fn highlight_code_line(text: &str, comment_prefixes: &[&str], theme: &Theme) -> Vec<Span<'static>> {
    let c = &theme.colors;
    let bg = c.code_background.to_color();
    let base = Style::default().fg(c.code_foreground.to_color()).bg(bg);
    let kw_style = Style::default().fg(c.code_keyword.to_color()).bg(bg);
    let str_style = Style::default().fg(c.code_string.to_color()).bg(bg);
    let com_style = Style::default().fg(c.code_comment.to_color()).bg(bg);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut pending = String::new();
    let flush = |spans: &mut Vec<Span<'static>>, pending: &mut String| {
        if !pending.is_empty() {
            spans.push(Span::styled(std::mem::take(pending), base));
        }
    };

    let mut it = text.char_indices().peekable();
    while let Some(&(byte_idx, ch)) = it.peek() {
        // Line comment → rest of the line.
        if comment_prefixes
            .iter()
            .any(|p| text[byte_idx..].starts_with(p))
        {
            flush(&mut spans, &mut pending);
            spans.push(Span::styled(text[byte_idx..].to_string(), com_style));
            break;
        }
        // String literal.
        if ch == '"' || ch == '\'' || ch == '`' {
            flush(&mut spans, &mut pending);
            let quote = ch;
            let start = byte_idx;
            it.next(); // opening quote
            let mut end = text.len();
            let mut escaped = false;
            while let Some(&(bi, ci)) = it.peek() {
                it.next();
                end = bi + ci.len_utf8();
                if escaped {
                    escaped = false;
                } else if ci == '\\' {
                    escaped = true;
                } else if ci == quote {
                    break;
                }
            }
            spans.push(Span::styled(text[start..end].to_string(), str_style));
            continue;
        }
        // Identifier / keyword.
        if ch.is_alphanumeric() || ch == '_' {
            let start = byte_idx;
            let mut end = byte_idx + ch.len_utf8();
            it.next();
            while let Some(&(bi, ci)) = it.peek() {
                if ci.is_alphanumeric() || ci == '_' {
                    end = bi + ci.len_utf8();
                    it.next();
                } else {
                    break;
                }
            }
            let word = &text[start..end];
            if is_keyword(word) {
                flush(&mut spans, &mut pending);
                spans.push(Span::styled(word.to_string(), kw_style));
            } else {
                pending.push_str(word);
            }
            continue;
        }
        // Anything else (whitespace, punctuation) → default run.
        pending.push(ch);
        it.next();
    }
    flush(&mut spans, &mut pending);
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse with the dark theme at a typical width, returning the bare lines
    /// (most tests here assert on text/structure, not the preformatted flag).
    fn md(input: &str) -> Vec<Line<'static>> {
        parse_markdown(input, &Theme::dark(), 80)
            .into_iter()
            .map(|ml| ml.line)
            .collect()
    }

    /// Flatten all spans in all lines into a single string.
    fn lines_to_text(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn wide_table_fits_narrow_viewport() {
        // #136: a 3-column table whose natural width far exceeds a narrow
        // viewport must shrink to fit, not overflow and clip at the edge.
        let width = 16;
        let lines = parse_markdown(
            "| aaaa | bbbb | cccc |\n|---|---|---|\n| aaaaaaaa | bbbbbbbb | cccccccc |\n",
            &Theme::dark(),
            width,
        );
        for ml in &lines {
            let w: usize = ml.line.spans.iter().map(|s| s.content.width()).sum();
            assert!(w <= width, "table row width {w} exceeds viewport {width}");
        }
    }

    #[test]
    fn test_plain_text() {
        let lines = md("Hello, world!");
        assert!(!lines.is_empty());
        assert!(lines_to_text(&lines).contains("Hello, world!"));
    }

    #[test]
    fn test_heading_levels() {
        let lines = md("# H1\n## H2\n### H3");
        let text = lines_to_text(&lines);
        assert!(text.contains("H1"));
        assert!(text.contains("H2"));
        assert!(text.contains("H3"));
        assert!(lines.len() >= 3);
    }

    #[test]
    fn line_hanging_indent_aligns_under_list_marker() {
        let theme = Theme::dark();
        fn find<'a>(lines: &'a [Line<'static>], needle: &str) -> &'a Line<'static> {
            lines
                .iter()
                .find(|l| lines_to_text(std::slice::from_ref(l)).contains(needle))
                .expect("line present")
        }

        // Bulleted item: continuations hang under the text after "• "
        // (2-cell nesting indent + 2-cell marker).
        let bullet = md("- Alpha item");
        assert_eq!(
            line_hanging_indent(find(&bullet, "Alpha"), &theme),
            4,
            "bullet: 2 indent + 2 marker"
        );

        // Numbered item: the marker "1. " is 3 cells wide.
        let numbered = md("1. First item");
        assert_eq!(
            line_hanging_indent(find(&numbered, "First"), &theme),
            5,
            "numbered: 2 indent + 3 marker"
        );

        // Ordinary paragraph: no marker, no leading indent, so no hang.
        let para = md("Just a sentence.");
        assert_eq!(
            line_hanging_indent(find(&para, "sentence"), &theme),
            0,
            "paragraph: flush to the gutter"
        );
    }

    #[test]
    fn test_code_block() {
        let lines = md("```rust\nfn main() {}\n```");
        let text = lines_to_text(&lines);
        assert!(text.contains("fn main() {}"));
        assert!(text.contains("rust"));
    }

    #[test]
    fn code_block_lines_tagged_with_code_background() {
        let lines = md("```rust\nfn main() {}\n```");
        let code_bg = Theme::dark().colors.code_background.to_color();
        // The line carrying the code body must be flagged via its base style
        // background (this is what the chat renderer keys off to skip wrap).
        assert!(
            lines.iter().any(|l| l.style.bg == Some(code_bg)
                && l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .contains("fn main")),
            "code body line must carry the code_background marker"
        );
    }

    #[test]
    fn code_block_highlights_keywords() {
        let lines = md("```rust\nfn main() {}\n```");
        let kw = Theme::dark().colors.code_keyword.to_color();
        // "fn" should be styled with the keyword color.
        let fn_styled_as_keyword = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content.as_ref() == "fn" && s.style.fg == Some(kw))
        });
        assert!(
            fn_styled_as_keyword,
            "`fn` should be highlighted as a keyword"
        );
    }

    #[test]
    fn code_block_preserves_indentation() {
        let lines = md("```rust\n    indented();\n```");
        // The leading 4 spaces must survive (no whitespace collapse).
        assert!(
            lines.iter().any(|l| l
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .starts_with("    indented")),
            "code indentation must be preserved verbatim"
        );
    }

    #[test]
    fn test_code_block_no_lang() {
        let lines = md("```\nsome code\n```");
        assert!(lines_to_text(&lines).contains("some code"));
    }

    #[test]
    fn test_inline_code_has_no_padding() {
        let lines = md("Use `cargo build` to compile");
        let code_bg = Theme::dark().colors.code_background.to_color();
        // The inline-code span must be exactly "cargo build" — not the old
        // " cargo build " with padding spaces baked into the highlight.
        let tight = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.style.bg == Some(code_bg) && s.content.as_ref() == "cargo build")
        });
        assert!(
            tight,
            "inline code should be tight (no surrounding padding spaces)"
        );
    }

    #[test]
    fn test_unordered_list() {
        let lines = md("- Item 1\n- Item 2\n- Item 3");
        let text = lines_to_text(&lines);
        assert!(text.contains("Item 1"));
        assert!(text.contains("•"));
    }

    #[test]
    fn test_ordered_list_preserves_numbers() {
        let lines = md("1. First\n2. Second\n3. Third");
        let text = lines_to_text(&lines);
        assert!(text.contains("1. First"));
        assert!(text.contains("2. Second"));
        assert!(!text.contains("• First"));
    }

    #[test]
    fn loose_list_item_body_hangs_under_item_text() {
        // A 2nd+ paragraph inside a list item must align under the item's text
        // (hanging indent), not fall back flush to column 0.
        let lines = md("- **Finding** — verified\n\n  Body paragraph explaining the finding.");
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            rendered
                .iter()
                .any(|l| l.starts_with("  • ") && l.contains("Finding")),
            "marker line should carry the bullet + indent"
        );
        let body = rendered
            .iter()
            .find(|l| l.contains("Body paragraph"))
            .expect("body line present");
        // 4 cols of hanging indent: 2 (depth) + 2 ("• " marker width), aligning
        // the body under the item text rather than flush at column 0.
        assert_eq!(
            body, "    Body paragraph explaining the finding.",
            "continuation paragraph must hang-indent under the item text"
        );
    }

    #[test]
    fn test_nested_list() {
        let lines = md("- Outer\n  - Inner");
        let text = lines_to_text(&lines);
        assert!(text.contains("Outer"));
        assert!(text.contains("Inner"));
    }

    #[test]
    fn test_bold_and_italic() {
        let lines = md("**bold** and *italic*");
        let text = lines_to_text(&lines);
        assert!(text.contains("bold"));
        assert!(text.contains("italic"));
    }

    #[test]
    fn test_link_shows_text_and_url() {
        let lines = md("[click here](https://example.com)");
        let text = lines_to_text(&lines);
        assert!(text.contains("click here"));
        // The destination is appended (dimmed) so the user can see where it goes.
        assert!(text.contains("https://example.com"));
    }

    #[test]
    fn test_autolink_does_not_duplicate_url() {
        // When the visible text already is the URL, don't append it twice.
        let lines = md("<https://example.com>");
        let text = lines_to_text(&lines);
        assert_eq!(text.matches("https://example.com").count(), 1);
    }

    #[test]
    fn test_blockquote() {
        let lines = md("> Quoted text");
        let text = lines_to_text(&lines);
        assert!(text.contains("Quoted text"));
        assert!(text.contains("│"));
    }

    #[test]
    fn test_horizontal_rule() {
        let lines = md("above\n\n---\n\nbelow");
        let text = lines_to_text(&lines);
        assert!(text.contains("above"));
        assert!(text.contains("below"));
        // The rule renders as a run of box-drawing dashes.
        assert!(text.contains("───"), "thematic break should render a rule");
    }

    #[test]
    fn test_table() {
        let lines = md("| Header1 | Header2 |\n|---------|--------|\n| Cell1   | Cell2  |");
        let text = lines_to_text(&lines);
        assert!(text.contains("Header1"));
        assert!(text.contains("Cell1"));
        assert!(text.contains("|"));
    }

    #[test]
    fn test_strikethrough() {
        let lines = md("~~deleted~~");
        assert!(lines_to_text(&lines).contains("deleted"));
    }

    #[test]
    fn test_empty_input() {
        assert!(md("").is_empty());
    }

    #[test]
    fn test_multiple_paragraphs() {
        let lines = md("Paragraph 1\n\nParagraph 2");
        let text = lines_to_text(&lines);
        assert!(text.contains("Paragraph 1"));
        assert!(text.contains("Paragraph 2"));
    }

    #[test]
    fn highlight_code_line_marks_strings_and_comments() {
        let theme = Theme::dark();
        let spans = highlight_code_line("let s = \"hi\"; // note", &["//"], &theme);
        let str_color = theme.colors.code_string.to_color();
        let com_color = theme.colors.code_comment.to_color();
        assert!(
            spans
                .iter()
                .any(|s| s.content.contains("\"hi\"") && s.style.fg == Some(str_color)),
            "string literal must use the string color"
        );
        assert!(
            spans
                .iter()
                .any(|s| s.content.contains("// note") && s.style.fg == Some(com_color)),
            "trailing comment must use the comment color"
        );
    }

    /// Tables with CJK cells align because column widths are display-cell
    /// based, not byte based.
    #[test]
    fn table_column_widths_use_display_cells() {
        let lines = md("| Name | Score |\n|------|-------|\n| 你好 | 100   |\n| ab   | 50    |");
        let mut cjk_row_width = 0usize;
        let mut ascii_row_width = 0usize;
        for line in &lines {
            let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            if rendered.contains("你好") {
                cjk_row_width = rendered.width();
            } else if rendered.contains("ab") && rendered.contains("|") {
                ascii_row_width = rendered.width();
            }
        }
        assert!(cjk_row_width > 0, "did not find the CJK body row");
        assert!(ascii_row_width > 0, "did not find the ASCII body row");
        assert_eq!(
            cjk_row_width, ascii_row_width,
            "CJK and ASCII rows must have equal display width to align"
        );
    }

    #[test]
    fn table_lines_flagged_preformatted_prose_is_not() {
        // Table rows must be flagged preformatted so the chat renderer doesn't
        // word-wrap them (which would collapse the column padding); prose must not.
        let out = parse_markdown(
            "Intro paragraph.\n\n| A | B |\n|---|---|\n| 1 | 2 |",
            &Theme::dark(),
            80,
        );
        let para = out
            .iter()
            .find(|ml| ml.line.spans.iter().any(|s| s.content.contains("Intro")))
            .expect("paragraph present");
        assert!(!para.preformatted, "prose must word-wrap normally");
        let table_rows: Vec<_> = out
            .iter()
            .filter(|ml| {
                ml.line
                    .spans
                    .first()
                    .is_some_and(|s| s.content.starts_with('|'))
            })
            .collect();
        assert!(!table_rows.is_empty(), "table should render rows");
        assert!(
            table_rows.iter().all(|ml| ml.preformatted),
            "every table line must be preformatted"
        );
    }

    #[test]
    fn code_lines_flagged_preformatted() {
        let out = parse_markdown("```\nlet x = 1;\n```", &Theme::dark(), 80);
        assert!(
            out.iter()
                .filter(|ml| ml.line.spans.iter().any(|s| s.content.contains("let x")))
                .all(|ml| ml.preformatted),
            "code-block lines must be preformatted"
        );
    }

    #[test]
    fn wide_table_wraps_cells_to_fit() {
        // A table wider than the viewport wraps cell text within columns rather
        // than overflowing. Every rendered table line must fit `width`, and no
        // cell content is lost.
        let width = 30;
        let out = parse_markdown(
            "| Item | Detail |\n|------|--------|\n| one | a very long cell that cannot fit on a single line at this width |",
            &Theme::dark(),
            width,
        );
        let mut saw_table = false;
        for ml in &out {
            let rendered: String = ml.line.spans.iter().map(|s| s.content.as_ref()).collect();
            if rendered.starts_with('|') {
                saw_table = true;
                assert!(
                    rendered.width() <= width,
                    "table line must fit width {width}, got {} for {rendered:?}",
                    rendered.width()
                );
            }
        }
        assert!(saw_table, "table should have rendered");
        // No content lost: every word of the long cell appears across the wraps.
        let all: String = out
            .iter()
            .map(|ml| {
                ml.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(" ");
        for word in ["very", "long", "cell", "cannot", "single", "width"] {
            assert!(all.contains(word), "wrapped table lost the word {word:?}");
        }
    }
}
