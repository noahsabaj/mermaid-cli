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
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, MouseEventKind},
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

use super::conversation::{ConversationHistory, ConversationManager};

/// Entries the mouse wheel scrolls the picker viewport per notch. The wheel
/// moves the *viewport*; the arrow keys move the *selection*.
const WHEEL_STEP: usize = 3;

/// Terminal rows each session block occupies (title + meta + a blank spacer).
/// The viewport fits `list.height / ROWS_PER_ENTRY` entries.
const ROWS_PER_ENTRY: usize = 3;

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
    manager: &ConversationManager,
    now: DateTime<Local>,
) -> Result<Option<ConversationHistory>> {
    if entries.is_empty() {
        println!("No previous conversations found in this directory.");
        return Ok(None);
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Enable mouse capture so the wheel arrives as real scroll events rather
    // than the alternate-screen's arrow-key translation (which would otherwise
    // move the selection instead of scrolling the viewport).
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = SelectorState::new(entries);
    let result = run_selector(&mut terminal, &mut state, manager, now);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
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
    /// First visible entry (index into the filtered set). The mouse wheel moves
    /// this freely; arrow-key selection clamps it so `selected` stays visible.
    scroll_offset: usize,
    /// Entries that fit in the list viewport — set by `render` each frame so the
    /// follow-selection clamp in `move_up`/`move_down` knows the window size.
    viewport_entries: usize,
    /// When `Some`, a delete of this *entries* index is awaiting a y/N confirm.
    pending_delete: Option<usize>,
}

impl SelectorState {
    pub fn new(entries: Vec<SessionEntry>) -> Self {
        Self {
            entries,
            query: String::new(),
            selected: 0,
            scroll_offset: 0,
            viewport_entries: 0,
            pending_delete: None,
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
            // Follow: if the selection dropped below the viewport, scroll to it.
            let visible = self.viewport_entries.max(1);
            if self.selected >= self.scroll_offset + visible {
                self.scroll_offset = self.selected + 1 - visible;
            }
        }
    }

    fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            // Follow: if the selection rose above the viewport, scroll to it.
            if self.selected < self.scroll_offset {
                self.scroll_offset = self.selected;
            }
        }
    }

    /// A typed character extends the query; selection + viewport reset to the
    /// top so the highlight can never point past the shrunken filtered set.
    fn push_query(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
        self.scroll_offset = 0;
    }

    fn pop_query(&mut self) {
        self.query.pop();
        self.selected = 0;
        self.scroll_offset = 0;
    }

    /// Mouse wheel: scroll the viewport without touching the selection. The
    /// upper bound is clamped in `render`, which knows the viewport height.
    fn scroll_viewport_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(WHEEL_STEP);
    }

    fn scroll_viewport_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(WHEEL_STEP);
    }

    /// Arm a delete of the highlighted entry (awaits a y/N confirm). Stores the
    /// *entries* index so a filtered-view change can't misredirect it.
    fn request_delete(&mut self) {
        self.pending_delete = self.filtered().get(self.selected).copied();
    }

    fn cancel_delete(&mut self) {
        self.pending_delete = None;
    }

    fn take_pending_delete(&mut self) -> Option<usize> {
        self.pending_delete.take()
    }

    /// Drop an entry after it's deleted on disk, re-clamping the selection into
    /// the new (smaller) filtered set.
    fn remove_entry(&mut self, entries_idx: usize) {
        if entries_idx >= self.entries.len() {
            return;
        }
        self.entries.remove(entries_idx);
        let n = self.filtered().len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
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
    manager: &ConversationManager,
    now: DateTime<Local>,
) -> Result<Option<ConversationHistory>> {
    loop {
        terminal.draw(|f| render(f, state, now))?;

        match event::read()? {
            Event::Key(key) => {
                // A pending delete captures the next key: `y` confirms, anything
                // else cancels — intercepted here so neither falls through to
                // the search box.
                if state.pending_delete.is_some() {
                    if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                        if let Some(idx) = state.take_pending_delete() {
                            let id = state.entries[idx].history.id.clone();
                            if manager.delete_conversation(&id).is_ok() {
                                state.remove_entry(idx);
                                if state.entries.is_empty() {
                                    return Ok(None);
                                }
                            }
                        }
                    } else {
                        state.cancel_delete();
                    }
                    continue;
                }
                match key.code {
                    KeyCode::Esc => return Ok(None),
                    KeyCode::Enter => return Ok(state.current().map(|e| e.history.clone())),
                    KeyCode::Down => state.move_down(),
                    KeyCode::Up => state.move_up(),
                    // Del arms a confirm to delete the highlighted session. Must
                    // be a non-typing key — printable chars are search input.
                    KeyCode::Delete => state.request_delete(),
                    KeyCode::Backspace => state.pop_query(),
                    // Everything printable is search input — there is no vim-style
                    // `j`/`k`/`q` navigation, or it would be swallowed as typing.
                    KeyCode::Char(c) => state.push_query(c),
                    _ => {},
                }
            },
            // The wheel scrolls the viewport; the selection stays put.
            Event::Mouse(m) => match m.kind {
                MouseEventKind::ScrollUp => state.scroll_viewport_up(),
                MouseEventKind::ScrollDown => state.scroll_viewport_down(),
                _ => {},
            },
            _ => {},
        }
    }
}

// Palette — kept local (this mini-TUI runs before the themed app is built) but
// chosen to read like the main UI: cyan accent, gray meta, dim hints.
const ACCENT: Color = Color::Cyan;
const META: Color = Color::Gray;
const DIM: Color = Color::DarkGray;

/// Draw the picker: header, search line, project name, one two-line block per
/// filtered session (windowed by the scroll offset), then the key hints.
///
/// Takes `&mut SelectorState` because the viewport height is only known here:
/// it records how many entries fit (for follow-selection) and clamps the
/// wheel-driven scroll offset to the valid range.
pub fn render(f: &mut Frame, state: &mut SelectorState, now: DateTime<Local>) {
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

    // Viewport math first (this mutates `state`, so it must precede the
    // immutable-borrow draws below): how many 3-line entry blocks fit, recorded
    // for follow-selection, and the wheel-driven offset clamped to range.
    let filtered = state.filtered();
    let visible = (list.height as usize / ROWS_PER_ENTRY).max(1);
    state.viewport_entries = visible;
    let max_offset = filtered.len().saturating_sub(visible);
    if state.scroll_offset > max_offset {
        state.scroll_offset = max_offset;
    }
    let scroll_offset = state.scroll_offset;
    let selected = state.selected;
    let pending_delete = state.pending_delete;

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

    let lines: Vec<Line> = if filtered.is_empty() {
        vec![Line::from(Span::styled(
            "  No sessions match your search.",
            Style::default().fg(DIM),
        ))]
    } else {
        filtered
            .iter()
            .enumerate()
            .skip(scroll_offset)
            .take(visible)
            .flat_map(|(row, &idx)| {
                let entry = &state.entries[idx];
                let is_selected = row == selected;
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

    // Hints line, or the delete confirm prompt when one is armed.
    let hints_line = if let Some(idx) = pending_delete {
        let name: String = state
            .entries
            .get(idx)
            .map(|e| e.history.title.chars().take(40).collect::<String>())
            .unwrap_or_default();
        Line::from(Span::styled(
            format!("Delete \"{name}\"?  y confirms · any other key cancels"),
            Style::default().fg(Color::Yellow),
        ))
    } else {
        Line::from(Span::styled(
            "↑↓ select · type to search · del delete · enter resume · esc cancel",
            Style::default().fg(DIM),
        ))
    };
    f.render_widget(Paragraph::new(hints_line), hints);
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
    // Session lineage: mark a branched-from session (dormant until fork/rewind
    // lands, but the field is persisted now).
    if entry.history.forked_from.is_some() {
        bits.push("forked".to_string());
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
            compactions: Vec::new(),
            input_history: VecDeque::new(),
            git_branch: branch.map(str::to_string),
            safety_mode: None,
            plan: None,
            last_token_usage: None,
            cumulative_token_usage: crate::domain::TokenUsageTotals::default(),
            context_usage: None,
            forked_from: None,
            parent_session: None,
            cli_version: None,
            git_sha: None,
            tasks: crate::domain::TaskStore::default(),
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
        let mut state = SelectorState::new(vec![
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
        terminal.draw(|f| render(f, &mut state, now)).unwrap();
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
        terminal.draw(|f| render(f, &mut state, now)).unwrap();
        let buf = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf[(x, y)].symbol());
            }
        }
        assert!(text.contains("No sessions match"), "{text}");
    }

    /// Render the current buffer to a plain string for assertions.
    fn dump(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn arrowing_past_the_viewport_scrolls_to_follow_selection() {
        let now = at(2026, 7, 2, 12, 0);
        // 8 entries × 3 rows; a short terminal fits only ~2, so moving the
        // selection to the bottom must scroll the window to keep it visible.
        let mut state = SelectorState::new(
            (0..8)
                .map(|i| entry(&format!("Session {i}"), None, now, 100))
                .collect(),
        );
        let backend = TestBackend::new(80, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        // First frame records the viewport height for the follow clamp.
        terminal.draw(|f| render(f, &mut state, now)).unwrap();
        for _ in 0..7 {
            state.move_down();
        }
        terminal.draw(|f| render(f, &mut state, now)).unwrap();
        let text = dump(&terminal);
        assert!(
            text.contains("> Session 7"),
            "selection followed into view:\n{text}"
        );
        assert!(
            !text.contains("Session 0"),
            "top entries scrolled away:\n{text}"
        );
    }

    #[test]
    fn wheel_scrolls_viewport_without_moving_selection() {
        let now = at(2026, 7, 2, 12, 0);
        let mut state = SelectorState::new(
            (0..8)
                .map(|i| entry(&format!("Session {i}"), None, now, 100))
                .collect(),
        );
        let backend = TestBackend::new(80, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut state, now)).unwrap();

        state.scroll_viewport_down(); // wheel down: viewport moves, selection doesn't
        terminal.draw(|f| render(f, &mut state, now)).unwrap();
        let text = dump(&terminal);
        assert_eq!(state.selected, 0, "the wheel must not move the selection");
        assert!(
            !text.contains("Session 0"),
            "viewport scrolled past the top:\n{text}"
        );
    }

    #[test]
    fn delete_flow_arms_confirm_then_drops_the_entry() {
        let now = at(2026, 7, 2, 12, 0);
        let mut state = SelectorState::new(vec![
            entry("keep", None, now, 1),
            entry("gone", None, now, 1),
        ]);
        state.move_down(); // highlight "gone" (entries index 1)
        state.request_delete();
        assert_eq!(
            state.pending_delete,
            Some(1),
            "armed on the highlighted entry"
        );
        // The manager's on-disk delete lives in run_selector; here we drive the
        // state mutation that follows a confirmed delete.
        let idx = state.take_pending_delete().expect("pending");
        state.remove_entry(idx);
        assert_eq!(state.entries.len(), 1);
        assert_eq!(
            state.current().map(|e| e.history.title.as_str()),
            Some("keep"),
            "selection re-clamped onto the surviving entry"
        );
    }
}
