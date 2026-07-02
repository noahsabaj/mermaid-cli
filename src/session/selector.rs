//! The `--resume` conversation picker: a searchable list of this
//! directory's past sessions, styled to match the main TUI (borderless,
//! muted-gray meta text) rather than the old bordered box.
//!
//! Structure mirrors the render layer's split: [`SelectorState`] is pure
//! (query + selection + filtering, unit-tested) and [`render`] draws it to a
//! `Frame` (asserted via ratatui's `TestBackend`). Only [`select_conversation`]
//! touches the real terminal.

use anyhow::Result;
use chrono::{DateTime, Local};
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::io;
use std::path::Path;

use super::conversation::ConversationHistory;

/// One row in the picker: a conversation plus its on-disk size (shown in the
/// meta line; not stored on the history itself).
pub struct SessionEntry {
    pub history: ConversationHistory,
    pub size_bytes: u64,
}

/// Show the searchable resume picker and return the chosen conversation, or
/// `None` if the user cancelled. `now` is injected so the relative-time labels
/// are testable and match the caller's clock.
pub fn select_conversation(
    entries: Vec<SessionEntry>,
    now: DateTime<Local>,
) -> Result<Option<ConversationHistory>> {
    if entries.is_empty() {
        println!("No previous conversations found in this directory.");
        return Ok(None);
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = SelectorState::new(entries);
    let result = run_selector(&mut terminal, &mut state, now);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

/// Pure picker state: the entries, the live search query, and the highlighted
/// row (an index into the *filtered* view).
pub struct SelectorState {
    entries: Vec<SessionEntry>,
    query: String,
    /// Selection index within the current filtered set.
    selected: usize,
}

impl SelectorState {
    pub fn new(entries: Vec<SessionEntry>) -> Self {
        Self {
            entries,
            query: String::new(),
            selected: 0,
        }
    }

    /// Indices into `entries` whose title or branch match the query
    /// (case-insensitive substring). An empty query matches everything.
    /// Order is preserved from `entries` (already newest-first from the
    /// caller).
    fn filtered(&self) -> Vec<usize> {
        if self.query.is_empty() {
            return (0..self.entries.len()).collect();
        }
        let needle = self.query.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| entry_matches(&e.history, &needle))
            .map(|(i, _)| i)
            .collect()
    }

    /// The entry currently highlighted, if the filtered set is non-empty.
    fn current(&self) -> Option<&SessionEntry> {
        self.filtered()
            .get(self.selected)
            .map(|&i| &self.entries[i])
    }

    fn move_down(&mut self) {
        let n = self.filtered().len();
        if n > 0 && self.selected + 1 < n {
            self.selected += 1;
        }
    }

    fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    /// A typed character extends the query; selection resets to the top so the
    /// highlight can never point past the shrunken filtered set.
    fn push_query(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
    }

    fn pop_query(&mut self) {
        self.query.pop();
        self.selected = 0;
    }
}

/// True when the query is a case-insensitive substring of the title or the
/// git branch. `needle` must already be lowercased.
fn entry_matches(history: &ConversationHistory, needle: &str) -> bool {
    history.title.to_lowercase().contains(needle)
        || history
            .git_branch
            .as_deref()
            .is_some_and(|b| b.to_lowercase().contains(needle))
}

fn run_selector(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut SelectorState,
    now: DateTime<Local>,
) -> Result<Option<ConversationHistory>> {
    loop {
        terminal.draw(|f| render(f, state, now))?;

        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Esc => return Ok(None),
                KeyCode::Enter => return Ok(state.current().map(|e| e.history.clone())),
                KeyCode::Down => state.move_down(),
                KeyCode::Up => state.move_up(),
                KeyCode::Backspace => state.pop_query(),
                // Everything printable is search input — there is no vim-style
                // `j`/`k`/`q` navigation, or it would be swallowed as typing.
                KeyCode::Char(c) => state.push_query(c),
                _ => {},
            }
        }
    }
}

// Palette — kept local (this mini-TUI runs before the themed app is built) but
// chosen to read like the main UI: cyan accent, gray meta, dim hints.
const ACCENT: Color = Color::Cyan;
const META: Color = Color::Gray;
const DIM: Color = Color::DarkGray;

/// Draw the picker: header, search line, project name, one two-line block per
/// filtered session, then the key hints.
pub fn render(f: &mut Frame, state: &SelectorState, now: DateTime<Local>) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // "Resume session"
            Constraint::Length(1), // blank
            Constraint::Length(1), // search
            Constraint::Length(1), // blank
            Constraint::Length(1), // project name
            Constraint::Length(1), // blank
            Constraint::Min(3),    // list
            Constraint::Length(1), // hints
        ]);
    let [title, _s1, search, _s2, project, _s3, list, hints] = f.area().layout(&layout);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Resume session",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))),
        title,
    );

    // Search line: the query, or a muted placeholder when empty.
    let search_line = if state.query.is_empty() {
        Line::from(Span::styled("  Search…", Style::default().fg(DIM)))
    } else {
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(state.query.clone(), Style::default().fg(Color::White)),
            Span::styled("▏", Style::default().fg(ACCENT)),
        ])
    };
    f.render_widget(Paragraph::new(search_line), search);

    // Project name (this picker is scoped to one directory).
    if let Some(name) = state
        .entries
        .first()
        .map(|e| project_name(&e.history.project_path))
    {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                name,
                Style::default().fg(META).add_modifier(Modifier::BOLD),
            ))),
            project,
        );
    }

    let filtered = state.filtered();
    let lines: Vec<Line> = if filtered.is_empty() {
        vec![Line::from(Span::styled(
            "  No sessions match your search.",
            Style::default().fg(DIM),
        ))]
    } else {
        filtered
            .iter()
            .enumerate()
            .flat_map(|(row, &idx)| {
                let entry = &state.entries[idx];
                let is_selected = row == state.selected;
                let (marker, title_style) = if is_selected {
                    (
                        "> ",
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    )
                } else {
                    ("  ", Style::default().fg(Color::White))
                };
                let title_line = Line::from(vec![
                    Span::styled(marker, Style::default().fg(ACCENT)),
                    Span::styled(entry.history.title.clone(), title_style),
                ]);
                let meta_line = Line::from(vec![Span::styled(
                    format!("  {}", meta_label(entry, now)),
                    Style::default().fg(META),
                )]);
                [title_line, meta_line, Line::from("")]
            })
            .collect()
    };
    f.render_widget(Paragraph::new(lines), list);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "↑↓ select · type to search · enter resume · esc cancel",
            Style::default().fg(DIM),
        ))),
        hints,
    );
}

/// The gray meta line under a title: "relative-time · branch · size", with the
/// branch omitted when unknown.
fn meta_label(entry: &SessionEntry, now: DateTime<Local>) -> String {
    let mut bits = vec![humanize_relative(now, entry.history.updated_at)];
    if let Some(branch) = &entry.history.git_branch
        && !branch.is_empty()
    {
        bits.push(branch.clone());
    }
    bits.push(humanize_size(entry.size_bytes));
    bits.join(" · ")
}

/// Last path component of a project path, for the picker header.
fn project_name(project_path: &str) -> String {
    Path::new(project_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| project_path.to_string())
}

/// Coarse "N units ago" label. Singular/plural aware; caps at months so an
/// ancient session doesn't read as "412 days ago".
fn humanize_relative(now: DateTime<Local>, then: DateTime<Local>) -> String {
    let secs = (now - then).num_seconds().max(0);
    let (n, unit) = if secs < 45 {
        return "just now".to_string();
    } else if secs < 3600 {
        (secs / 60, "minute")
    } else if secs < 86_400 {
        (secs / 3600, "hour")
    } else if secs < 7 * 86_400 {
        (secs / 86_400, "day")
    } else if secs < 30 * 86_400 {
        (secs / (7 * 86_400), "week")
    } else {
        (secs / (30 * 86_400), "month")
    };
    let n = n.max(1);
    if n == 1 {
        format!("1 {unit} ago")
    } else {
        format!("{n} {unit}s ago")
    }
}

/// Human-readable byte count: `512B`, `23.1KB`, `1.3MB`.
fn humanize_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes}B")
    } else if b < MB {
        format!("{:.1}KB", b / KB)
    } else {
        format!("{:.1}MB", b / MB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::VecDeque;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    fn history(title: &str, branch: Option<&str>, updated: DateTime<Local>) -> ConversationHistory {
        ConversationHistory {
            id: "20260101_000000_000".to_string(),
            title: title.to_string(),
            messages: Vec::new(),
            model_name: "ollama/test".to_string(),
            project_path: "/home/nsabaj/Development/source-clone".to_string(),
            created_at: updated,
            updated_at: updated,
            total_tokens: None,
            compactions: Vec::new(),
            input_history: VecDeque::new(),
            git_branch: branch.map(str::to_string),
        }
    }

    fn entry(
        title: &str,
        branch: Option<&str>,
        updated: DateTime<Local>,
        size: u64,
    ) -> SessionEntry {
        SessionEntry {
            history: history(title, branch, updated),
            size_bytes: size,
        }
    }

    #[test]
    fn humanize_relative_scales_and_pluralizes() {
        let now = at(2026, 7, 2, 12, 0);
        assert_eq!(humanize_relative(now, now), "just now");
        assert_eq!(
            humanize_relative(now, at(2026, 7, 2, 11, 58)),
            "2 minutes ago"
        );
        assert_eq!(humanize_relative(now, at(2026, 7, 2, 11, 0)), "1 hour ago");
        assert_eq!(humanize_relative(now, at(2026, 7, 1, 12, 0)), "1 day ago");
        assert_eq!(humanize_relative(now, at(2026, 6, 29, 12, 0)), "3 days ago");
        assert_eq!(humanize_relative(now, at(2026, 6, 20, 12, 0)), "1 week ago");
        assert_eq!(
            humanize_relative(now, at(2026, 5, 1, 12, 0)),
            "2 months ago"
        );
        // A clock skew where `then` is in the future must not underflow.
        assert_eq!(humanize_relative(now, at(2026, 7, 2, 12, 30)), "just now");
    }

    #[test]
    fn humanize_size_picks_unit() {
        assert_eq!(humanize_size(512), "512B");
        assert_eq!(humanize_size(23_100), "22.6KB");
        assert_eq!(humanize_size(1_367_426), "1.3MB");
    }

    #[test]
    fn filtering_matches_title_and_branch_case_insensitively() {
        let now = at(2026, 7, 2, 12, 0);
        let mut state = SelectorState::new(vec![
            entry("Examine the workspace", Some("main"), now, 100),
            entry("Fix the parser", Some("feature/parser"), now, 200),
            entry("Unrelated", Some("main"), now, 300),
        ]);
        // Empty query → everything.
        assert_eq!(state.filtered().len(), 3);
        // Title substring, case-insensitive.
        state.push_query('E');
        state.push_query('x');
        assert_eq!(state.filtered(), vec![0]);
        // Branch match.
        state.query.clear();
        for c in "parser".chars() {
            state.push_query(c);
        }
        assert_eq!(state.filtered(), vec![1]);
        // No match → empty, and selection was reset so `current` is None.
        state.query.clear();
        for c in "zzz".chars() {
            state.push_query(c);
        }
        assert!(state.filtered().is_empty());
        assert!(state.current().is_none());
    }

    #[test]
    fn navigation_is_clamped_to_filtered_set() {
        let now = at(2026, 7, 2, 12, 0);
        let mut state =
            SelectorState::new(vec![entry("A", None, now, 1), entry("B", None, now, 2)]);
        state.move_up(); // already at top — no underflow
        assert_eq!(state.selected, 0);
        state.move_down();
        assert_eq!(state.selected, 1);
        state.move_down(); // at bottom — no overflow past 2 items
        assert_eq!(state.selected, 1);
        assert_eq!(state.current().unwrap().history.title, "B");
    }

    #[test]
    fn render_shows_claude_code_style_rows() {
        let now = at(2026, 7, 2, 12, 0);
        let state = SelectorState::new(vec![
            entry(
                "Examine the workspace",
                Some("main"),
                at(2026, 7, 2, 10, 0),
                1_367_426,
            ),
            entry(
                "Older session",
                Some("master"),
                at(2026, 6, 29, 12, 0),
                18_400_000,
            ),
        ]);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &state, now)).unwrap();
        let buf = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(text.contains("Resume session"), "header:\n{text}");
        assert!(text.contains("Search"), "search placeholder:\n{text}");
        assert!(text.contains("source-clone"), "project name:\n{text}");
        assert!(text.contains("Examine the workspace"), "title:\n{text}");
        // The meta line: relative time · branch · size.
        assert!(text.contains("2 hours ago · main · 1.3MB"), "meta:\n{text}");
        assert!(
            text.contains("3 days ago · master · 17.5MB"),
            "meta2:\n{text}"
        );
        // Selected row marker on the first entry.
        assert!(text.contains("> Examine the workspace"), "marker:\n{text}");
        assert!(text.contains("esc cancel"), "hints:\n{text}");
    }

    #[test]
    fn render_empty_filter_shows_no_match_message() {
        let now = at(2026, 7, 2, 12, 0);
        let mut state = SelectorState::new(vec![entry("A", None, now, 1)]);
        for c in "zzz".chars() {
            state.push_query(c);
        }
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &state, now)).unwrap();
        let buf = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf[(x, y)].symbol());
            }
        }
        assert!(text.contains("No sessions match"), "{text}");
    }
}
