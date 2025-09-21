use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, HeadingLevel, CodeBlockKind};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

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

    for event in parser {
        match event {
            Event::Start(tag) => {
                let new_style = match tag {
                    Tag::Heading { level, .. } => {
                        // Start new line for headers
                        if !current_line_spans.is_empty() {
                            lines.push(Line::from(current_line_spans.clone()));
                            current_line_spans.clear();
                        }

                        // Add header prefix and style based on level
                        let (prefix, style) = match level {
                            HeadingLevel::H1 => ("# ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                            HeadingLevel::H2 => ("## ", Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)),
                            HeadingLevel::H3 => ("### ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                            _ => ("#### ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                        };
                        current_line_spans.push(Span::styled(prefix, style));
                        style
                    }
                    Tag::Emphasis => {
                        style_stack.last().unwrap().add_modifier(Modifier::ITALIC)
                    }
                    Tag::Strong => {
                        style_stack.last().unwrap().add_modifier(Modifier::BOLD)
                    }
                    Tag::Strikethrough => {
                        style_stack.last().unwrap().add_modifier(Modifier::CROSSED_OUT)
                    }
                    Tag::CodeBlock(kind) => {
                        in_code_block = true;
                        code_block_content.clear();
                        // Start new line for code block
                        if !current_line_spans.is_empty() {
                            lines.push(Line::from(current_line_spans.clone()));
                            current_line_spans.clear();
                        }
                        // Add code block header
                        let lang = match kind {
                            CodeBlockKind::Fenced(lang) => lang.to_string(),
                            CodeBlockKind::Indented => "".to_string(),
                        };
                        if !lang.is_empty() {
                            lines.push(Line::from(vec![
                                Span::styled("```", Style::default().fg(Color::DarkGray)),
                                Span::styled(lang, Style::default().fg(Color::Magenta)),
                            ]));
                        } else {
                            lines.push(Line::from(vec![
                                Span::styled("```", Style::default().fg(Color::DarkGray)),
                            ]));
                        }
                        Style::default().fg(Color::Gray)
                    }
                    Tag::List(_) => {
                        list_depth += 1;
                        if !current_line_spans.is_empty() {
                            lines.push(Line::from(current_line_spans.clone()));
                            current_line_spans.clear();
                        }
                        *style_stack.last().unwrap()
                    }
                    Tag::Item => {
                        // Add bullet point with indentation
                        let indent = "  ".repeat(list_depth.saturating_sub(1));
                        current_line_spans.push(Span::raw(indent));
                        current_line_spans.push(Span::styled("• ", Style::default().fg(Color::Yellow)));
                        *style_stack.last().unwrap()
                    }
                    Tag::Link { .. } => {
                        current_line_spans.push(Span::styled("[", Style::default().fg(Color::Blue)));
                        Style::default().fg(Color::Blue).add_modifier(Modifier::UNDERLINED)
                    }
                    Tag::BlockQuote(_) => {
                        if !current_line_spans.is_empty() {
                            lines.push(Line::from(current_line_spans.clone()));
                            current_line_spans.clear();
                        }
                        current_line_spans.push(Span::styled("│ ", Style::default().fg(Color::DarkGray)));
                        Style::default().fg(Color::Gray).add_modifier(Modifier::ITALIC)
                    }
                    _ => *style_stack.last().unwrap(),
                };
                style_stack.push(new_style);
            }
            Event::End(tag) => {
                style_stack.pop();
                match tag {
                    TagEnd::Heading(_) | TagEnd::Paragraph | TagEnd::Item => {
                        if !current_line_spans.is_empty() {
                            lines.push(Line::from(current_line_spans.clone()));
                            current_line_spans.clear();
                        }
                    }
                    TagEnd::CodeBlock => {
                        in_code_block = false;
                        // Render code block content
                        for line in code_block_content.lines() {
                            lines.push(Line::from(vec![
                                Span::styled(line.to_string(), Style::default().fg(Color::Gray)),
                            ]));
                        }
                        lines.push(Line::from(vec![
                            Span::styled("```", Style::default().fg(Color::DarkGray)),
                        ]));
                        code_block_content.clear();
                    }
                    TagEnd::List(_) => {
                        list_depth = list_depth.saturating_sub(1);
                    }
                    TagEnd::Link => {
                        current_line_spans.push(Span::styled("]", Style::default().fg(Color::Blue)));
                    }
                    TagEnd::BlockQuote(_) => {
                        if !current_line_spans.is_empty() {
                            lines.push(Line::from(current_line_spans.clone()));
                            current_line_spans.clear();
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(text) => {
                if in_code_block {
                    code_block_content.push_str(&text);
                } else {
                    let style = *style_stack.last().unwrap();
                    current_line_spans.push(Span::styled(text.to_string(), style));
                }
            }
            Event::Code(code) => {
                let style = Style::default()
                    .fg(Color::Yellow)
                    .bg(Color::Rgb(40, 40, 40));
                current_line_spans.push(Span::styled(format!(" {} ", code), style));
            }
            Event::SoftBreak | Event::HardBreak => {
                if !current_line_spans.is_empty() {
                    lines.push(Line::from(current_line_spans.clone()));
                    current_line_spans.clear();
                }
            }
            _ => {}
        }
    }

    // Add any remaining spans as a line
    if !current_line_spans.is_empty() {
        lines.push(Line::from(current_line_spans));
    }

    lines
}