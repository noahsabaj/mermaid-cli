use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::macros::{line, span};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone)]
struct ListState {
    next_number: Option<u64>,
}

/// Parse markdown and convert to styled ratatui Lines
pub fn parse_markdown(input: &str) -> Vec<Line<'static>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);

    let parser = Parser::new_ext(input, options);
    let mut lines = Vec::new();
    let mut current_line_spans = Vec::new();
    let mut style_stack = vec![Style::default()];
    let mut in_code_block = false;
    let mut code_block_content = String::new();
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
                        // Start new line for headers
                        if !current_line_spans.is_empty() {
                            lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                        }

                        // Add blank line before heading (except for first heading)
                        if !lines.is_empty() {
                            lines.push(Line::from(""));
                        }

                        // Apply style based on level (without visible prefix)
                        match level {
                            HeadingLevel::H1 => Style::new().fg(Color::Cyan).bold(),
                            HeadingLevel::H2 => Style::new().fg(Color::Blue).bold(),
                            HeadingLevel::H3 => Style::new().fg(Color::Green).bold(),
                            _ => Style::new().fg(Color::Yellow).bold(),
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
                        // Start new line for code block
                        if !current_line_spans.is_empty() {
                            lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                        }
                        // Show language label if present, otherwise just a blank separator
                        let lang = match kind {
                            CodeBlockKind::Fenced(lang) => lang.to_string(),
                            CodeBlockKind::Indented => "".to_string(),
                        };
                        if !lang.is_empty() {
                            lines.push(line![span!(Color::DarkGray; &lang)]);
                        }
                        Style::default().fg(Color::Gray)
                    },
                    Tag::List(start) => {
                        list_stack.push(ListState { next_number: start });
                        if !current_line_spans.is_empty() {
                            lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                        }
                        style_stack.last().copied().unwrap_or_default()
                    },
                    Tag::Item => {
                        // Add marker with 2-space base indentation plus nesting levels.
                        // depth=1: 2 spaces, depth=2: 4 spaces, etc.
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
                        current_line_spans
                            .push(Span::styled(marker, Style::default().fg(Color::Yellow)));
                        style_stack.last().copied().unwrap_or_default()
                    },
                    Tag::Table(_alignments) => {
                        in_table = true;
                        table_rows.clear();
                        table_header_len = 0;
                        // Flush any pending spans
                        if !current_line_spans.is_empty() {
                            lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                        }
                        style_stack.last().copied().unwrap_or_default()
                    },
                    Tag::TableHead => {
                        current_row.clear();
                        style_stack.last().copied().unwrap_or_default()
                    },
                    Tag::TableRow => {
                        current_row.clear();
                        style_stack.last().copied().unwrap_or_default()
                    },
                    Tag::TableCell => {
                        current_cell.clear();
                        style_stack.last().copied().unwrap_or_default()
                    },
                    Tag::Link { .. } => {
                        current_line_spans
                            .push(Span::styled("[", Style::default().fg(Color::Blue)));
                        Style::default()
                            .fg(Color::Blue)
                            .add_modifier(Modifier::UNDERLINED)
                    },
                    Tag::BlockQuote(_) => {
                        if !current_line_spans.is_empty() {
                            lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                        }
                        current_line_spans
                            .push(Span::styled("│ ", Style::default().fg(Color::DarkGray)));
                        Style::default()
                            .fg(Color::Gray)
                            .add_modifier(Modifier::ITALIC)
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
                        // Don't add blank line here - let lists flow directly from headings
                        // Blank line before next heading is added by Tag::Heading
                    },
                    TagEnd::Paragraph | TagEnd::Item => {
                        if !current_line_spans.is_empty() {
                            lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                        }
                    },
                    TagEnd::CodeBlock => {
                        in_code_block = false;
                        // Render code block content (no fence markers)
                        for line in code_block_content.lines() {
                            lines.push(Line::from(vec![Span::styled(
                                line.to_string(),
                                Style::default().fg(Color::Gray),
                            )]));
                        }
                        code_block_content.clear();
                    },
                    TagEnd::List(_) => {
                        let _ = list_stack.pop();
                        // Add blank line after list ends (when returning to depth 0)
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
                        // Compute column widths in **display cells**, not bytes.
                        // CJK / emoji cells (3 bytes per codepoint, 2 cells)
                        // would otherwise inflate widths by ~3× and misalign
                        // every other row. Padding below also uses cell-based
                        // computation rather than format!'s char-count default.
                        let num_cols = table_rows.iter().map(|r| r.len()).max().unwrap_or(0);
                        let mut col_widths = vec![0usize; num_cols];
                        for row in &table_rows {
                            for (i, cell) in row.iter().enumerate() {
                                if i < num_cols {
                                    col_widths[i] = col_widths[i].max(cell.width());
                                }
                            }
                        }
                        // Minimum column width of 3
                        for w in &mut col_widths {
                            *w = (*w).max(3);
                        }

                        let border_style = Style::default().fg(Color::DarkGray);
                        let header_style = Style::default().fg(Color::Cyan).bold();
                        let cell_style = Style::default().fg(Color::White);

                        for (row_idx, row) in table_rows.iter().enumerate() {
                            let mut spans = Vec::new();
                            spans.push(Span::styled("| ", border_style));
                            for (col_idx, cell) in row.iter().enumerate() {
                                let width = col_widths.get(col_idx).copied().unwrap_or(3);
                                // Pad to `width` display cells (not chars or
                                // bytes). `{:<width$}` would pad to chars,
                                // which is wrong for CJK (each char is 2 cells
                                // but counted as 1 char by the formatter).
                                let cell_w = cell.width();
                                let padding = width.saturating_sub(cell_w);
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

                            // Add separator after header row. `width` is now
                            // in cells; "-" is 1 byte / 1 cell so repeating
                            // produces exactly `width` cells of dashes.
                            if row_idx == 0 && table_header_len > 0 {
                                let mut sep_spans = Vec::new();
                                sep_spans.push(Span::styled("|-", border_style));
                                for (col_idx, _) in row.iter().enumerate() {
                                    let width = col_widths.get(col_idx).copied().unwrap_or(3);
                                    let dashes = "-".repeat(width);
                                    sep_spans.push(Span::styled(dashes, border_style));
                                    sep_spans.push(Span::styled("-|-", border_style));
                                }
                                lines.push(Line::from(sep_spans));
                            }
                        }

                        // Blank line after table
                        lines.push(Line::from(""));
                        table_rows.clear();
                    },
                    TagEnd::Link => {
                        current_line_spans
                            .push(Span::styled("]", Style::default().fg(Color::Blue)));
                    },
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
                    let style = Style::default()
                        .fg(Color::Yellow)
                        .bg(Color::Rgb(40, 40, 40));
                    current_line_spans.push(Span::styled(format!(" {} ", code), style));
                }
            },
            Event::SoftBreak | Event::HardBreak => {
                if !current_line_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_line_spans)));
                }
            },
            _ => {},
        }
    }

    // Add any remaining spans as a line
    if !current_line_spans.is_empty() {
        lines.push(Line::from(current_line_spans));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: flatten all spans in all lines into a single string
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
        let lines = parse_markdown("Hello, world!");
        assert!(!lines.is_empty());
        assert!(lines_to_text(&lines).contains("Hello, world!"));
    }

    #[test]
    fn test_heading_levels() {
        let lines = parse_markdown("# H1\n## H2\n### H3");
        let text = lines_to_text(&lines);
        assert!(text.contains("H1"));
        assert!(text.contains("H2"));
        assert!(text.contains("H3"));

        // Headings should have different colors (just check they parse without panic)
        assert!(lines.len() >= 3);
    }

    #[test]
    fn test_code_block() {
        let input = "```rust\nfn main() {}\n```";
        let lines = parse_markdown(input);
        let text = lines_to_text(&lines);
        assert!(text.contains("fn main() {}"));
        // Language label should appear
        assert!(text.contains("rust"));
    }

    #[test]
    fn test_code_block_no_lang() {
        let input = "```\nsome code\n```";
        let lines = parse_markdown(input);
        let text = lines_to_text(&lines);
        assert!(text.contains("some code"));
    }

    #[test]
    fn test_inline_code() {
        let lines = parse_markdown("Use `cargo build` to compile");
        let text = lines_to_text(&lines);
        assert!(text.contains("cargo build"));
    }

    #[test]
    fn test_unordered_list() {
        let input = "- Item 1\n- Item 2\n- Item 3";
        let lines = parse_markdown(input);
        let text = lines_to_text(&lines);
        assert!(text.contains("Item 1"));
        assert!(text.contains("Item 2"));
        assert!(text.contains("Item 3"));
        // Should have bullet characters
        assert!(text.contains("•"));
    }

    #[test]
    fn test_ordered_list_preserves_numbers() {
        let input = "1. First\n2. Second\n3. Third";
        let lines = parse_markdown(input);
        let text = lines_to_text(&lines);
        assert!(text.contains("1. First"));
        assert!(text.contains("2. Second"));
        assert!(text.contains("3. Third"));
        assert!(!text.contains("• First"));
    }

    #[test]
    fn test_nested_list() {
        let input = "- Outer\n  - Inner";
        let lines = parse_markdown(input);
        let text = lines_to_text(&lines);
        assert!(text.contains("Outer"));
        assert!(text.contains("Inner"));
    }

    #[test]
    fn test_bold_and_italic() {
        let lines = parse_markdown("**bold** and *italic*");
        let text = lines_to_text(&lines);
        assert!(text.contains("bold"));
        assert!(text.contains("italic"));
    }

    #[test]
    fn test_link() {
        let lines = parse_markdown("[click here](https://example.com)");
        let text = lines_to_text(&lines);
        assert!(text.contains("click here"));
        assert!(text.contains("["));
        assert!(text.contains("]"));
    }

    #[test]
    fn test_blockquote() {
        let lines = parse_markdown("> Quoted text");
        let text = lines_to_text(&lines);
        assert!(text.contains("Quoted text"));
        assert!(text.contains("│"));
    }

    #[test]
    fn test_table() {
        let input = "| Header1 | Header2 |\n|---------|--------|\n| Cell1   | Cell2  |";
        let lines = parse_markdown(input);
        let text = lines_to_text(&lines);
        assert!(text.contains("Header1"));
        assert!(text.contains("Cell1"));
        assert!(text.contains("|"));
    }

    #[test]
    fn test_strikethrough() {
        let lines = parse_markdown("~~deleted~~");
        let text = lines_to_text(&lines);
        assert!(text.contains("deleted"));
    }

    #[test]
    fn test_empty_input() {
        let lines = parse_markdown("");
        assert!(lines.is_empty());
    }

    #[test]
    fn test_multiple_paragraphs() {
        let lines = parse_markdown("Paragraph 1\n\nParagraph 2");
        let text = lines_to_text(&lines);
        assert!(text.contains("Paragraph 1"));
        assert!(text.contains("Paragraph 2"));
    }

    /// Tables with CJK cells used to misalign because `cell.len()` reported
    /// 3 bytes for each 2-cell character. After the fix, column widths are
    /// computed in display cells and padding emits enough trailing spaces
    /// to make every row visually line up.
    #[test]
    fn table_column_widths_use_display_cells() {
        let input = "| Name | Score |\n|------|-------|\n| 你好 | 100   |\n| ab   | 50    |";
        let lines = parse_markdown(input);

        // Find the body row containing "你好" and the row containing "ab"
        // (skipping header + separator).
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
            "CJK and ASCII rows must have equal display width for the table to align"
        );
    }
}
