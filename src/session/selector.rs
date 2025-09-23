use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame, Terminal,
};
use std::io;

use super::conversation::ConversationHistory;

/// Show a selection UI for choosing a conversation to resume
pub fn select_conversation(conversations: Vec<ConversationHistory>) -> Result<Option<ConversationHistory>> {
    if conversations.is_empty() {
        println!("No previous conversations found in this directory.");
        return Ok(None);
    }

    // If there's only one conversation, return it directly
    if conversations.len() == 1 {
        return Ok(Some(conversations.into_iter().next().unwrap()));
    }

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app state
    let mut app = ConversationSelector {
        conversations,
        selected: 0,
    };

    // Run the UI loop
    let result = run_selector(&mut terminal, &mut app);

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

struct ConversationSelector {
    conversations: Vec<ConversationHistory>,
    selected: usize,
}

fn run_selector(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut ConversationSelector,
) -> Result<Option<ConversationHistory>> {
    loop {
        terminal.draw(|f| render_selector(f, app))?;

        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    return Ok(None);
                }
                KeyCode::Enter => {
                    let selected = app.conversations[app.selected].clone();
                    return Ok(Some(selected));
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if app.selected < app.conversations.len() - 1 {
                        app.selected += 1;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if app.selected > 0 {
                        app.selected -= 1;
                    }
                }
                KeyCode::Home => {
                    app.selected = 0;
                }
                KeyCode::End => {
                    app.selected = app.conversations.len() - 1;
                }
                _ => {}
            }
        }
    }
}

fn render_selector(f: &mut Frame, app: &ConversationSelector) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(f.area());

    // Title
    let title = Paragraph::new("Select a conversation to resume")
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::ALL).title(" Mermaid - Resume Session "));
    f.render_widget(title, chunks[0]);

    // Conversation list
    let items: Vec<ListItem> = app
        .conversations
        .iter()
        .enumerate()
        .map(|(i, conv)| {
            let style = if i == app.selected {
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            let content = vec![
                Line::from(vec![
                    Span::styled(&conv.title, style),
                ]),
                Line::from(vec![
                    Span::styled(
                        format!(
                            "  {} | {} messages | Model: {}",
                            conv.updated_at.format("%Y-%m-%d %H:%M"),
                            conv.messages.len(),
                            conv.model_name
                        ),
                        style.fg(Color::Gray),
                    ),
                ]),
            ];

            ListItem::new(content)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Previous Conversations "),
        )
        .highlight_style(Style::default())
        .highlight_symbol("");

    f.render_widget(list, chunks[1]);

    // Help text
    let help = vec![
        Line::from(vec![
            Span::raw("Up/k: Up  Down/j: Down  "),
            Span::styled("Enter", Style::default().fg(Color::Green)),
            Span::raw(": Select  "),
            Span::styled("q/Esc", Style::default().fg(Color::Red)),
            Span::raw(": Cancel"),
        ]),
    ];
    let help_widget = Paragraph::new(help)
        .style(Style::default().fg(Color::DarkGray))
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(help_widget, chunks[2]);
}