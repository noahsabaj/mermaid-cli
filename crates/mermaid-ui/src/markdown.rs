use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::node::{Line, Span};
use crate::theme::{StyleToken, Theme, ThemeToken};

/// A parsed markdown line plus whether it is preformatted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownLine {
    pub line: Line,
    pub preformatted: bool,
}

#[derive(Debug, Clone)]
struct ListState {
    next_number: Option<u64>,
    cont_indent: String,
}

fn list_marker_style() -> StyleToken {
    StyleToken::new().fg(ThemeToken::TextSecondary)
}

/// Hanging indent (display cells) for hard-wrapping `line`.
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

/// Parse a single-line or inline markdown string into a styled [`Line`].
#[must_use]
pub fn parse_markdown_inline(input: &str, _theme: &Theme, base_style: StyleToken) -> Line {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);

    let link_style = StyleToken::new().fg(ThemeToken::Info).underline();

    let parser = Parser::new_ext(input, options);
    let mut spans: Vec<Span> = Vec::new();
    let mut style_stack = vec![base_style];

    for event in parser {
        match event {
            Event::Start(tag) => {
                let current = style_stack.last().copied().unwrap_or(base_style);
                let new_style = match tag {
                    Tag::Emphasis => current.italic(),
                    Tag::Strong => current.bold(),
                    Tag::Strikethrough => {
                        let mut s = current;
                        s.strikethrough = true;
                        s
                    },
                    Tag::Link { .. } => link_style,
                    _ => current,
                };
                style_stack.push(new_style);
            },
            Event::End(_) if style_stack.len() > 1 => {
                style_stack.pop();
            },
            Event::Text(text) => {
                let style = style_stack.last().copied().unwrap_or(base_style);
                spans.push(Span::styled(text.to_string(), style));
            },
            Event::Code(code) => {
                let style = StyleToken::new()
                    .fg(ThemeToken::CodeForeground)
                    .bg(ThemeToken::CodeBackground);
                spans.push(Span::styled(code.to_string(), style));
            },
            Event::SoftBreak | Event::HardBreak => {
                let style = style_stack.last().copied().unwrap_or(base_style);
                spans.push(Span::styled(" ".to_string(), style));
            },
            _ => {},
        }
    }

    Line::from(spans)
}

struct MarkdownParserState {
    lines: Vec<Line>,
    current_line_spans: Vec<Span>,
    style_stack: Vec<StyleToken>,
    in_code_block: bool,
    code_block_content: String,
    code_block_lang: String,
    current_link_url: Option<String>,
    list_stack: Vec<ListState>,
    in_table: bool,
    table_rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
    table_header_len: usize,
    table_line_indices: std::collections::HashSet<usize>,
}

impl MarkdownParserState {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            current_line_spans: Vec::new(),
            style_stack: vec![StyleToken::default()],
            in_code_block: false,
            code_block_content: String::new(),
            code_block_lang: String::new(),
            current_link_url: None,
            list_stack: Vec::new(),
            in_table: false,
            table_rows: Vec::new(),
            current_row: Vec::new(),
            current_cell: String::new(),
            table_header_len: 0,
            table_line_indices: std::collections::HashSet::new(),
        }
    }

    fn flush_current_line(&mut self) {
        if !self.current_line_spans.is_empty() {
            self.lines
                .push(Line::from(std::mem::take(&mut self.current_line_spans)));
        }
    }
}

fn start_heading(state: &mut MarkdownParserState, level: HeadingLevel) -> StyleToken {
    state.flush_current_line();
    if !state.lines.is_empty() {
        state.lines.push(Line::from(""));
    }
    match level {
        HeadingLevel::H1 => StyleToken::new().fg(ThemeToken::Header).bold(),
        HeadingLevel::H2 => StyleToken::new().fg(ThemeToken::Info).bold(),
        HeadingLevel::H3 => StyleToken::new().fg(ThemeToken::Success).bold(),
        _ => StyleToken::new().fg(ThemeToken::Warning).bold(),
    }
}

fn start_code_block(state: &mut MarkdownParserState, kind: CodeBlockKind) -> StyleToken {
    state.in_code_block = true;
    state.code_block_content.clear();
    state.flush_current_line();
    state.code_block_lang = match kind {
        CodeBlockKind::Fenced(lang) => lang.to_string(),
        CodeBlockKind::Indented => String::new(),
    };
    if !state.code_block_lang.is_empty() {
        state.lines.push(Line::from(Span::styled(
            state.code_block_lang.clone(),
            StyleToken::new().fg(ThemeToken::TextDisabled).italic(),
        )));
    }
    StyleToken::new().fg(ThemeToken::CodeForeground)
}

fn start_list_item(state: &mut MarkdownParserState) -> StyleToken {
    let indent = "  ".repeat(state.list_stack.len());
    let marker = if let Some(s) = state.list_stack.last_mut() {
        if let Some(current) = s.next_number {
            s.next_number = Some(current + 1);
            format!("{current}. ")
        } else {
            "• ".to_string()
        }
    } else {
        "• ".to_string()
    };
    let cont_indent = format!("{}{}", indent, " ".repeat(marker.as_str().width()));
    if let Some(s) = state.list_stack.last_mut() {
        s.cont_indent = cont_indent;
    }
    state.current_line_spans.push(Span::raw(indent));
    state
        .current_line_spans
        .push(Span::styled(marker, list_marker_style()));
    state.style_stack.last().copied().unwrap_or_default()
}

fn handle_start_tag(state: &mut MarkdownParserState, tag: Tag) {
    let new_style = match tag {
        Tag::Heading { level, .. } => start_heading(state, level),
        Tag::Emphasis => state
            .style_stack
            .last()
            .copied()
            .unwrap_or_default()
            .italic(),
        Tag::Strong => state.style_stack.last().copied().unwrap_or_default().bold(),
        Tag::Strikethrough => {
            let mut s = state.style_stack.last().copied().unwrap_or_default();
            s.strikethrough = true;
            s
        },
        Tag::CodeBlock(kind) => start_code_block(state, kind),
        Tag::List(start) => {
            state.list_stack.push(ListState {
                next_number: start,
                cont_indent: String::new(),
            });
            state.flush_current_line();
            state.style_stack.last().copied().unwrap_or_default()
        },
        Tag::Item => start_list_item(state),
        Tag::Paragraph => {
            if state.current_line_spans.is_empty()
                && let Some(s) = state.list_stack.last()
                && !s.cont_indent.is_empty()
            {
                state
                    .current_line_spans
                    .push(Span::raw(s.cont_indent.clone()));
            }
            state.style_stack.last().copied().unwrap_or_default()
        },
        Tag::Table(_alignments) => {
            state.in_table = true;
            state.table_rows.clear();
            state.table_header_len = 0;
            state.flush_current_line();
            state.style_stack.last().copied().unwrap_or_default()
        },
        Tag::TableHead | Tag::TableRow => {
            state.current_row.clear();
            state.style_stack.last().copied().unwrap_or_default()
        },
        Tag::TableCell => {
            state.current_cell.clear();
            state.style_stack.last().copied().unwrap_or_default()
        },
        Tag::Link { dest_url, .. } => {
            state.current_link_url = Some(dest_url.to_string());
            StyleToken::new().fg(ThemeToken::Info).underline()
        },
        Tag::BlockQuote(_) => {
            state.flush_current_line();
            state.current_line_spans.push(Span::styled(
                "│ ",
                StyleToken::new().fg(ThemeToken::TextDisabled),
            ));
            StyleToken::new().fg(ThemeToken::TextSecondary).italic()
        },
        _ => state.style_stack.last().copied().unwrap_or_default(),
    };
    state.style_stack.push(new_style);
}

fn handle_end_tag(state: &mut MarkdownParserState, tag: TagEnd, theme: &Theme, width: usize) {
    state.style_stack.pop();
    match tag {
        TagEnd::Heading(_) | TagEnd::Paragraph | TagEnd::Item | TagEnd::BlockQuote(_)
            if !state.current_line_spans.is_empty() =>
        {
            state.flush_current_line();
        },
        TagEnd::CodeBlock => {
            state.in_code_block = false;
            let prefixes = line_comment_prefixes(&state.code_block_lang);
            for line_text in state.code_block_content.lines() {
                let spans = highlight_code_line(line_text, prefixes);
                state.lines.push(Line::from(spans));
            }
            state.code_block_content.clear();
            state.code_block_lang.clear();
        },
        TagEnd::List(_) => {
            let _ = state.list_stack.pop();
            if state.list_stack.is_empty() {
                state.lines.push(Line::from(""));
            }
        },
        TagEnd::TableCell => {
            state
                .current_row
                .push(std::mem::take(&mut state.current_cell));
        },
        TagEnd::TableHead => {
            state.table_header_len = state.current_row.len();
            state
                .table_rows
                .push(std::mem::take(&mut state.current_row));
        },
        TagEnd::TableRow => {
            state
                .table_rows
                .push(std::mem::take(&mut state.current_row));
        },
        TagEnd::Table => {
            state.in_table = false;
            let from = state.lines.len();
            render_table(
                &mut state.lines,
                &state.table_rows,
                state.table_header_len,
                theme,
                width,
            );
            state.table_line_indices.extend(from..state.lines.len());
            state.table_rows.clear();
        },
        TagEnd::Link => {
            if let Some(url) = state.current_link_url.take() {
                let text: String = state
                    .current_line_spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect();
                if !url.is_empty() && !text.ends_with(&url) {
                    state.current_line_spans.push(Span::styled(
                        format!(" ({url})"),
                        StyleToken::new().fg(ThemeToken::TextDisabled),
                    ));
                }
            }
        },
        _ => {},
    }
}

/// Parse markdown into theme-styled lines, each flagged [`MarkdownLine::preformatted`]
/// when it must not be word-wrapped.
#[must_use]
pub fn parse_markdown(input: &str, theme: &Theme, width: usize) -> Vec<MarkdownLine> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);

    let parser = Parser::new_ext(input, options);
    let mut state = MarkdownParserState::new();

    for event in parser {
        match event {
            Event::Start(tag) => handle_start_tag(&mut state, tag),
            Event::End(tag) => handle_end_tag(&mut state, tag, theme, width),
            Event::Text(text) => {
                if state.in_code_block {
                    state.code_block_content.push_str(&text);
                } else if state.in_table {
                    state.current_cell.push_str(&text);
                } else {
                    let style = state.style_stack.last().copied().unwrap_or_default();
                    state
                        .current_line_spans
                        .push(Span::styled(text.to_string(), style));
                }
            },
            Event::Code(code) => {
                if state.in_table {
                    state.current_cell.push_str(&code);
                } else {
                    let style = StyleToken::new()
                        .fg(ThemeToken::CodeForeground)
                        .bg(ThemeToken::CodeBackground);
                    state
                        .current_line_spans
                        .push(Span::styled(code.to_string(), style));
                }
            },
            Event::Rule => {
                state.flush_current_line();
                state.lines.push(Line::from(Span::styled(
                    "─".repeat(40),
                    StyleToken::new().fg(ThemeToken::TextDisabled),
                )));
            },
            Event::SoftBreak | Event::HardBreak if !state.current_line_spans.is_empty() => {
                state.flush_current_line();
            },
            _ => {},
        }
    }

    state.flush_current_line();

    state
        .lines
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let is_code = line
                .spans
                .iter()
                .any(|s| s.style.bg == Some(ThemeToken::CodeBackground));
            MarkdownLine {
                preformatted: is_code || state.table_line_indices.contains(&i),
                line,
            }
        })
        .collect()
}

fn render_table(
    lines: &mut Vec<Line>,
    table_rows: &[Vec<String>],
    table_header_len: usize,
    _theme: &Theme,
    width: usize,
) {
    let num_cols = table_rows.iter().map(Vec::len).max().unwrap_or(0);
    if num_cols == 0 {
        return;
    }

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
                None => break,
            }
        }
    }

    let border_style = StyleToken::new().fg(ThemeToken::TextDisabled);
    let header_style = StyleToken::new().fg(ThemeToken::Header).bold();
    let cell_style = StyleToken::new().fg(ThemeToken::TextPrimary);

    for (row_idx, row) in table_rows.iter().enumerate() {
        let style = if row_idx == 0 && table_header_len > 0 {
            header_style
        } else {
            cell_style
        };
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
                let closer = if col + 1 == num_cols { "-|" } else { "-|-" };
                sep_spans.push(Span::styled(closer, border_style));
            }
            lines.push(Line::from(sep_spans));
        }
    }

    lines.push(Line::from(""));
}

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

fn highlight_code_line(text: &str, comment_prefixes: &[&str]) -> Vec<Span> {
    let base = StyleToken::new()
        .fg(ThemeToken::CodeForeground)
        .bg(ThemeToken::CodeBackground);
    let kw_style = StyleToken::new()
        .fg(ThemeToken::CodeKeyword)
        .bg(ThemeToken::CodeBackground);
    let str_style = StyleToken::new()
        .fg(ThemeToken::CodeString)
        .bg(ThemeToken::CodeBackground);
    let com_style = StyleToken::new()
        .fg(ThemeToken::CodeComment)
        .bg(ThemeToken::CodeBackground);

    let mut spans: Vec<Span> = Vec::new();
    let mut pending = String::new();
    let mut it = text.char_indices().peekable();

    while let Some(&(byte_idx, ch)) = it.peek() {
        if comment_prefixes
            .iter()
            .any(|p| text[byte_idx..].starts_with(p))
        {
            if !pending.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut pending), base));
            }
            spans.push(Span::styled(text[byte_idx..].to_string(), com_style));
            break;
        }
        if ch == '"' || ch == '\'' || ch == '`' {
            if !pending.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut pending), base));
            }
            let quote = ch;
            let start = byte_idx;
            it.next();
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
                if !pending.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut pending), base));
                }
                spans.push(Span::styled(word.to_string(), kw_style));
            } else {
                pending.push_str(word);
            }
            continue;
        }
        it.next();
        pending.push(ch);
    }

    if !pending.is_empty() {
        spans.push(Span::styled(pending, base));
    }

    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_markdown_headings_and_lists() {
        let theme = Theme::dark();
        let md = "# Title\n\n- item 1\n- item 2\n";
        let lines = parse_markdown(md, &theme, 80);
        assert!(!lines.is_empty());
    }

    #[test]
    fn parse_markdown_inline_code() {
        let theme = Theme::dark();
        let line = parse_markdown_inline("hello `code` world", &theme, StyleToken::default());
        assert_eq!(line.spans.len(), 3);
    }
}
