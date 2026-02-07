use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::macros::{line, span};

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
    let mut list_depth: usize = 0;

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
                        let style = match level {
                            HeadingLevel::H1 => Style::new().fg(Color::Cyan).bold(),
                            HeadingLevel::H2 => Style::new().fg(Color::Blue).bold(),
                            HeadingLevel::H3 => Style::new().fg(Color::Green).bold(),
                            _ => Style::new().fg(Color::Yellow).bold(),
                        };
                        style
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
                    Tag::List(_) => {
                        list_depth += 1;
                        if !current_line_spans.is_empty() {
                            lines.push(Line::from(std::mem::take(&mut current_line_spans)));

                        }
                        style_stack.last().copied().unwrap_or_default()
                    },
                    Tag::Item => {
                        // Add bullet point with 2-space base indentation plus nesting levels
                        // list_depth=1: 2 spaces, list_depth=2: 4 spaces, etc.
                        let indent = "  ".repeat(list_depth);
                        current_line_spans.push(Span::raw(indent));
                        current_line_spans
                            .push(Span::styled("• ", Style::default().fg(Color::Yellow)));
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
                        list_depth = list_depth.saturating_sub(1);
                        // Add blank line after list ends (when returning to depth 0)
                        if list_depth == 0 {
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
                        // Compute column widths
                        let num_cols = table_rows.iter().map(|r| r.len()).max().unwrap_or(0);
                        let mut col_widths = vec![0usize; num_cols];
                        for row in &table_rows {
                            for (i, cell) in row.iter().enumerate() {
                                if i < num_cols {
                                    col_widths[i] = col_widths[i].max(cell.len());
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
                                let padded = format!("{:<width$}", cell, width = width);
                                let style = if row_idx == 0 && table_header_len > 0 {
                                    header_style
                                } else {
                                    cell_style
                                };
                                spans.push(Span::styled(padded, style));
                                spans.push(Span::styled(" | ", border_style));
                            }
                            lines.push(Line::from(spans));

                            // Add separator after header row
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
