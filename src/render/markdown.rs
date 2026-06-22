use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::render::theme::Theme;

#[derive(Debug, Clone)]
struct ListState {
    next_number: Option<u64>,
}

/// Parse markdown and convert to theme-styled ratatui Lines.
///
/// Code-block lines are tagged by setting the returned `Line`'s base style
/// background to the theme's `code_background`. The chat renderer keys off
/// that to skip word-wrapping them (so indentation survives) and to know
/// they're pre-formatted. Inline code carries the background on the *span*,
/// not the line, so prose lines that merely contain `code` still word-wrap.
pub fn parse_markdown(input: &str, theme: &Theme) -> Vec<Line<'static>> {
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
    let marker_style = Style::new().fg(c.text_secondary.to_color());
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
    let mut list_stack: Vec<ListState> = Vec::new();

    // Table state
    let mut in_table = false;
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut current_row: Vec<String> = Vec::new();
    let mut current_cell = String::new();
    let mut table_header_len: usize = 0;

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
                        list_stack.push(ListState { next_number: start });
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
                        current_line_spans.push(Span::raw(indent));
                        current_line_spans.push(Span::styled(marker, marker_style));
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
                    Tag::Link { .. } => {
                        // Render the link text underlined in the accent color.
                        // (No raw URL — terminals can't follow it without OSC-8,
                        // and inlining it adds noise.)
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
                    TagEnd::Heading(_) => {
                        if !current_line_spans.is_empty() {
                            lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                        }
                    },
                    TagEnd::Paragraph | TagEnd::Item => {
                        if !current_line_spans.is_empty() {
                            lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                        }
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
                        render_table(&mut lines, &table_rows, table_header_len, theme);
                        table_rows.clear();
                    },
                    TagEnd::Link => {},
                    TagEnd::BlockQuote(_) => {
                        if !current_line_spans.is_empty() {
                            lines.push(Line::from(std::mem::take(&mut current_line_spans)));
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
            Event::SoftBreak | Event::HardBreak => {
                if !current_line_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                }
            },
            _ => {},
        }
    }

    if !current_line_spans.is_empty() {
        lines.push(Line::from(current_line_spans));
    }

    lines
}

/// Render the accumulated table rows into aligned, themed lines.
fn render_table(
    lines: &mut Vec<Line<'static>>,
    table_rows: &[Vec<String>],
    table_header_len: usize,
    theme: &Theme,
) {
    let c = &theme.colors;
    // Column widths in DISPLAY CELLS (CJK-safe), min 3.
    let num_cols = table_rows.iter().map(|r| r.len()).max().unwrap_or(0);
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

    let border_style = Style::default().fg(c.text_disabled.to_color());
    let header_style = Style::default().fg(c.header.to_color()).bold();
    let cell_style = Style::default().fg(c.text_primary.to_color());

    for (row_idx, row) in table_rows.iter().enumerate() {
        let mut spans = vec![Span::styled("| ", border_style)];
        for (col_idx, cell) in row.iter().enumerate() {
            let width = col_widths.get(col_idx).copied().unwrap_or(3);
            let padding = width.saturating_sub(cell.width());
            let padded = format!("{}{}", cell, " ".repeat(padding));
            let style = if row_idx == 0 && table_header_len > 0 {
                header_style
            } else {
                cell_style
            };
            spans.push(Span::styled(padded, style));
            spans.push(Span::styled(" | ", border_style));
        }
        lines.push(Line::from(spans));

        if row_idx == 0 && table_header_len > 0 {
            let mut sep_spans = vec![Span::styled("|-", border_style)];
            for (col_idx, _) in row.iter().enumerate() {
                let width = col_widths.get(col_idx).copied().unwrap_or(3);
                sep_spans.push(Span::styled("-".repeat(width), border_style));
                sep_spans.push(Span::styled("-|-", border_style));
            }
            lines.push(Line::from(sep_spans));
        }
    }

    lines.push(Line::from(""));
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

    /// Parse with the dark theme (tests only assert on text/structure).
    fn md(input: &str) -> Vec<Line<'static>> {
        parse_markdown(input, &Theme::dark())
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
    fn test_link_shows_text() {
        let lines = md("[click here](https://example.com)");
        let text = lines_to_text(&lines);
        assert!(text.contains("click here"));
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
}
