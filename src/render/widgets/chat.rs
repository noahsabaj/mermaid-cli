use crate::render::wrap::{wrap_styled_line, wrap_text_with_indent};
use std::hash::{Hash, Hasher};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, StatefulWidget, Widget},
};
use rustc_hash::FxHashMap;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::render::markdown::parse_markdown;
use crate::render::theme::Theme;
use mermaid_domain::{
    ActionDetails, ActionDisplay, ActionResult, QuestionAnswer, ToolMetadata, format_compact_count,
};
use mermaid_model::diff::{DiffLineKind, parse_diff_line};
use mermaid_model::models::ChatMessageKind;
use mermaid_model::models::{ChatMessage, MessageRole};

/// Entry in the click map: maps a content line to an image in chat history
#[derive(Debug, Clone)]
pub struct ImageClickTarget {
    /// Index into the DISPLAY message slice this frame rendered. The display
    /// slice can diverge from committed history (the continuation stitch hides
    /// nudges and merges bubbles), so this is only a fallback locator — prefer
    /// `image_number`.
    pub message_index: usize,
    /// Index into that display message's images vec
    pub image_index: usize,
    /// The image's stable global `[Image #N]` number, when it has one.
    /// Position-independent, so the reducer can resolve the click against
    /// committed history no matter how the display transcript was stitched.
    pub image_number: Option<u64>,
}

/// State for the chat widget
#[derive(Debug, Clone)]
pub struct ChatState {
    /// Manual scroll offset (only used when `is_user_scrolling` = true)
    scroll_offset: u16,
    /// Whether user is manually scrolling (not following bottom)
    is_user_scrolling: bool,
    /// Click map: content line number → image target (rebuilt every render)
    pub image_click_map: Vec<(u16, ImageClickTarget)>,
    /// Scroll position used in last render (for coordinate mapping)
    pub last_scroll_position: u16,
    /// Chat area rect from last render
    pub last_chat_area: Option<(u16, u16, u16, u16)>, // (x, y, width, height)
    /// Active drag-selection in CONTENT coordinates: `(anchor, cursor)` where
    /// each is `(content_line, col_cells)`. Highlight + copy derive from it.
    selection: Option<((usize, usize), (usize, usize))>,
    /// Plain text of each rendered content row, captured every frame so the
    /// selection can be extracted by display-cell range. Indexed by content
    /// line (the same index the selection uses).
    last_rendered_rows: Vec<String>,
    /// Memoized full-frame assembly (F31): the wrapped lines and image click
    /// map produced by the per-message render loop, keyed by a fingerprint of
    /// every input that determines them (message set, theme, width, reasoning
    /// toggle, day). An unchanged scrollback reuses this across frames instead
    /// of re-parsing, re-wrapping, and rebuilding the click map every frame.
    /// Replaced whenever the fingerprint changes.
    frame_memo: Option<FrameMemo>,
    /// Debug-only `(frame_key, full_content_hash)` from the previous frame,
    /// used to assert the O(1) key never misses a content change.
    #[cfg(debug_assertions)]
    debug_key_check: Option<(u64, u64)>,
}

/// One memoized chat-frame assembly (see `ChatState::frame_memo`). Holds the
/// lines *before* the per-frame selection highlight (which is selection-
/// dependent and applied to a clone each frame) plus the image click map, so a
/// frame whose inputs are unchanged skips the whole per-message render loop
/// (F31). Cloning is `O(total lines)`, but it replaces the markdown parse +
/// wrap + click-map rebuild the loop would otherwise redo every frame.
#[derive(Debug, Clone)]
struct FrameMemo {
    /// Fingerprint of the inputs that produced `lines` + `click_map`.
    key: u64,
    /// Assembled wrapped lines, before the per-frame selection highlight.
    lines: Vec<Line<'static>>,
    /// Image click map captured alongside `lines`.
    click_map: Vec<(u16, ImageClickTarget)>,
}

impl ChatState {
    /// Create a new chat state (starts in auto-follow mode)
    #[must_use]
    pub fn new() -> Self {
        Self {
            scroll_offset: 0,
            is_user_scrolling: false,
            image_click_map: Vec::new(),
            last_scroll_position: 0,
            last_chat_area: None,
            selection: None,
            last_rendered_rows: Vec::new(),
            frame_memo: None,
            #[cfg(debug_assertions)]
            debug_key_check: None,
        }
    }

    /// Get the scroll position for rendering
    /// `scroll_offset` represents distance from bottom, convert to ratatui scroll position
    #[must_use]
    pub fn get_scroll_position(&self, content_height: u16, viewport_height: u16) -> u16 {
        let max_scroll = content_height.saturating_sub(viewport_height);
        if self.is_user_scrolling {
            // Manual scroll: convert "distance from bottom" to scroll position
            // scroll_offset=0 → show bottom (max_scroll), scroll_offset=max → show top (0)
            let capped_offset = self.scroll_offset.min(max_scroll);
            max_scroll.saturating_sub(capped_offset)
        } else {
            // Auto-scroll: show bottom of content
            max_scroll
        }
    }

    /// Scroll viewport up (shows older messages further from bottom)
    pub fn scroll_up(&mut self, amount: u16) {
        self.is_user_scrolling = true;
        self.scroll_offset = self.scroll_offset.saturating_add(amount);
        // A selection's content-line anchors don't track scrolling; drop it
        // rather than leave a highlight stranded on the wrong rows.
        self.selection = None;
    }

    /// Scroll viewport down (shows newer messages closer to bottom)
    /// Automatically resumes auto-scroll when reaching the bottom
    pub fn scroll_down(&mut self, amount: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
        if self.scroll_offset == 0 {
            // Reached bottom — resume auto-follow mode
            self.is_user_scrolling = false;
        }
        self.selection = None;
    }

    /// Force resume auto-scroll mode (jump to bottom)
    pub fn resume_auto_scroll(&mut self) {
        self.is_user_scrolling = false;
        self.scroll_offset = 0;
    }

    /// Find an image click target at the given screen coordinates.
    /// Returns `Some((message_index`, `image_index`)) if an image indicator was clicked.
    #[must_use]
    pub fn find_image_at_screen_pos(&self, screen_row: u16) -> Option<&ImageClickTarget> {
        let (_, area_y, _, area_height) = self.last_chat_area?;

        // Check if click is within chat area
        if screen_row < area_y || screen_row >= area_y + area_height {
            return None;
        }

        // Convert screen row to content line
        let viewport_row = screen_row - area_y;
        let content_line = viewport_row + self.last_scroll_position;

        // Look up in click map
        self.image_click_map
            .iter()
            .find(|(line, _)| *line == content_line)
            .map(|(_, target)| target)
    }

    /// Map a screen `(row, col)` to content `(line, col_cells)`, or `None`
    /// when the point is outside the chat area. `col` is clamped to the chat
    /// area's left edge so a drag past the gutter still maps to column 0.
    fn screen_to_content(&self, screen_row: u16, screen_col: u16) -> Option<(usize, usize)> {
        let (area_x, area_y, _, area_height) = self.last_chat_area?;
        if screen_row < area_y || screen_row >= area_y + area_height {
            return None;
        }
        let content_line = (screen_row - area_y) as usize + self.last_scroll_position as usize;
        let col = screen_col.saturating_sub(area_x) as usize;
        Some((content_line, col))
    }

    /// Begin a drag selection at the given screen position (mouse-down).
    /// Anchors and cursor both start here; a plain click with no drag selects
    /// nothing.
    pub fn begin_selection(&mut self, screen_row: u16, screen_col: u16) {
        self.selection = self
            .screen_to_content(screen_row, screen_col)
            .map(|p| (p, p));
    }

    /// Extend the in-progress selection to the given screen position (drag).
    pub fn update_selection(&mut self, screen_row: u16, screen_col: u16) {
        if let Some((anchor, _)) = self.selection
            && let Some(cursor) = self.screen_to_content(screen_row, screen_col)
        {
            self.selection = Some((anchor, cursor));
        }
    }

    /// Extract the currently-selected text from the last rendered frame, or
    /// `None` if there's no selection or it's empty (e.g. a plain click).
    /// Walks the retained per-row text and slices each row by display cells so
    /// CJK / wide glyphs are never split mid-cell.
    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        let (a, b) = self.selection?;
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        if self.last_rendered_rows.is_empty() {
            return None;
        }
        let last = self.last_rendered_rows.len() - 1;
        let (start_line, start_col) = (start.0.min(last), start.1);
        let (end_line, end_col) = (end.0.min(last), end.1);

        let mut out = String::new();
        for line in start_line..=end_line {
            let row = &self.last_rendered_rows[line];
            let c0 = if line == start_line { start_col } else { 0 };
            let c1 = if line == end_line {
                end_col
            } else {
                usize::MAX
            };
            let mut piece = slice_by_cells(row, c0, c1).to_string();
            // Drop the rendered left margin (the "● "/"  " role/continuation
            // prefix — up to SELECT_MARGIN_CELLS cells of spaces) so copied
            // text is clean. Only spaces inside the margin zone [c0, MARGIN)
            // are removed, so a code line's own indentation is preserved.
            let mut margin = SELECT_MARGIN_CELLS.saturating_sub(c0);
            while margin > 0 && piece.starts_with(' ') {
                piece.remove(0);
                margin -= 1;
            }
            out.push_str(piece.trim_end());
            if line != end_line {
                out.push('\n');
            }
        }
        if out.is_empty() { None } else { Some(out) }
    }
}

/// Display-cell width of the role/continuation left margin ("● " or "  ")
/// that the renderer prepends to chat content lines. Stripped from copied
/// selections so the clipboard gets clean text.
const SELECT_MARGIN_CELLS: usize = 2;

/// Hard-wrap a pre-formatted (code) line at `width` display cells, preserving
/// every glyph (including whitespace) and each span's style. Continuation rows
/// get a `indent`-space hanging indent. Unlike `wrap_styled_line` this never
/// collapses runs of spaces, so code indentation and alignment survive.
fn wrap_preformatted(line: Line<'static>, width: usize, indent: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![line];
    }
    let total: usize = line.spans.iter().map(|s| s.content.width()).sum();
    if total <= width {
        return vec![line];
    }

    let base = line.style;
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    let mut on_first = true;

    for span in line.spans {
        let style = span.style;
        let mut buf = String::new();
        for ch in span.content.chars() {
            let cw = ch.width().unwrap_or(0);
            // Break before this char if it would overflow and the current row
            // already holds real content (beyond the continuation indent).
            let floor = if on_first { 0 } else { indent };
            if cur_w + cw > width && cur_w > floor {
                if !buf.is_empty() {
                    cur.push(Span::styled(std::mem::take(&mut buf), style));
                }
                out.push(Line::from(std::mem::take(&mut cur)).style(base));
                on_first = false;
                cur.push(Span::styled(" ".repeat(indent), base));
                cur_w = indent;
            }
            buf.push(ch);
            cur_w += cw;
        }
        if !buf.is_empty() {
            cur.push(Span::styled(buf, style));
        }
    }
    if !cur.is_empty() {
        out.push(Line::from(cur).style(base));
    }
    if out.is_empty() {
        vec![Line::from("").style(base)]
    } else {
        out
    }
}

/// Byte offset in `s` at the start of display-cell `target` (clamped to
/// `s.len()`). A wide glyph straddling `target` is kept whole on the right
/// side, so slicing never lands mid-character.
fn byte_at_cell(s: &str, target: usize) -> usize {
    if target == 0 {
        return 0;
    }
    let mut width = 0usize;
    for (idx, ch) in s.char_indices() {
        if width >= target {
            return idx;
        }
        width += ch.width().unwrap_or(0);
    }
    s.len()
}

/// Slice `s` to the display-cell range `[c0, c1)`.
fn slice_by_cells(s: &str, c0: usize, c1: usize) -> &str {
    let start = byte_at_cell(s, c0);
    let end = byte_at_cell(s, c1).max(start);
    &s[start..end]
}

/// Pad `s` on the right with spaces until it spans `cells` display columns,
/// measured with `UnicodeWidthStr::width` (not chars/bytes) so a CJK/emoji row's
/// background bar fills to the true visual edge instead of falling short (#101).
/// Never truncates — an already-too-wide `s` is returned unchanged.
fn pad_to_cells(s: &str, cells: usize) -> String {
    let w = s.width();
    if w >= cells {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + (cells - w));
    out.push_str(s);
    out.push_str(&" ".repeat(cells - w));
    out
}

/// The plain text of a rendered line (spans concatenated, styles dropped).
fn line_plain_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Saturating cast from a `usize` line counter to the `u16` ratatui scroll /
/// click-map coordinate. A scrollback longer than `u16::MAX` rows clamps to the
/// last addressable row instead of wrapping the index modulo 65536 (which a
/// plain `as u16` would do, corrupting both the scroll position and the image
/// click-map on a very long session) (F32).
fn clamp_to_u16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// Apply `hl` (merged onto each span's existing style) to display cells
/// `[c0, c1)` of `line`, splitting spans at the selection boundaries so the
/// highlight lands on exactly the selected glyphs.
fn highlight_line_cells(line: &mut Line<'static>, c0: usize, c1: usize, hl: Style) {
    let mut new_spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 2);
    let mut width = 0usize;
    for span in line.spans.drain(..) {
        let span_w = span.content.width();
        let (span_start, span_end) = (width, width + span_w);
        width = span_end;

        let ov0 = c0.max(span_start);
        let ov1 = c1.min(span_end);
        if ov1 <= ov0 {
            new_spans.push(span); // no overlap with the selection
            continue;
        }

        let s = span.content.as_ref();
        let b0 = byte_at_cell(s, ov0 - span_start);
        let b1 = byte_at_cell(s, ov1 - span_start);
        if b0 > 0 {
            new_spans.push(Span::styled(s[..b0].to_string(), span.style));
        }
        new_spans.push(Span::styled(s[b0..b1].to_string(), span.style.patch(hl)));
        if b1 < s.len() {
            new_spans.push(Span::styled(s[b1..].to_string(), span.style));
        }
    }
    line.spans = new_spans;
}

impl Default for ChatState {
    fn default() -> Self {
        Self::new()
    }
}

/// Props for `ChatWidget`
pub struct ChatWidget<'a> {
    pub messages: &'a [ChatMessage],
    pub theme: &'a Theme,
    /// Shared render cache: `(content, theme, width)` hash → fully wrapped,
    /// role-prefixed assistant lines. Caching the WRAPPED output (not just the
    /// markdown parse) keeps a committed message from being re-parsed *and*
    /// re-wrapped every frame — it's cloned from here instead (#134).
    pub wrapped_line_cache: &'a mut FxHashMap<u64, Vec<Line<'static>>>,
    /// O(1) identity of `messages` for the frame memo — see
    /// `render::chat_content_key`. Passed in rather than derived here because
    /// the conversation revision that makes it O(1) lives on `State`.
    pub content_key: u64,
    pub show_reasoning: bool,
    /// Blink phase for in-flight (`ActionResult::Running`) action headers,
    /// derived from `state.now` by the compose function. Ignored — including
    /// by the frame memo — when no message carries a running action, so idle
    /// frames don't reassemble twice a second.
    pub blink_on: bool,
}

/// Render assistant message content (markdown) into wrapped, role-prefixed
/// display lines.
///
/// Pure in its inputs — `(content, width, role prefix/color, theme)` — which is
/// exactly what lets the result be cached per message and reused across frames
/// without re-parsing or re-wrapping (#134). The cache key folds in content,
/// theme, and width; role prefix/color are constant on this (assistant-only)
/// path, so they need not be keyed.
fn wrap_assistant_content(
    content: &str,
    content_width: u16,
    role_prefix: &str,
    role_color: ratatui::style::Color,
    theme: &Theme,
) -> Vec<Line<'static>> {
    // Markdown content sits after the 2-cell message gutter.
    let md_width = (content_width as usize).saturating_sub(2);
    let parsed = parse_markdown(content, theme, md_width);

    let mut out: Vec<Line<'static>> = Vec::new();
    for (line_idx, parsed_line) in parsed.into_iter().enumerate() {
        // Code-block lines are tagged with the code background on their base
        // style (see markdown::parse_markdown). They're pre-formatted: don't
        // word-wrap (that collapses indentation) — let the Paragraph clip
        // overflow instead.
        let preformatted = parsed_line.preformatted;
        let base_style = parsed_line.line.style;

        // Continuation indent for wrapping: the 2-cell message gutter every line
        // carries, plus this line's own content-start column so a wrapped list
        // item's continuations hang under its text (after the marker) instead of
        // snapping back to the gutter.
        let continuation = if preformatted {
            2
        } else {
            2 + crate::render::markdown::line_hanging_indent(&parsed_line.line, theme)
        };

        // Add role indicator to first line or 2-space margin to others.
        let mut spans = if line_idx == 0 {
            vec![Span::styled(
                format!("{role_prefix} "),
                Style::new().fg(role_color).bold(),
            )]
        } else {
            vec![Span::raw("  ")]
        };
        spans.extend(parsed_line.line.spans);
        let new_line = Line::from(spans).style(base_style);

        if preformatted {
            // Code: hard-wrap preserving indentation (don't word-collapse) so
            // wide lines stay readable.
            out.extend(wrap_preformatted(new_line, content_width as usize, 2));
        } else {
            out.extend(wrap_styled_line(
                new_line,
                content_width as usize,
                continuation,
            ));
        }
    }
    out
}

/// `std::fmt::Write` shim that streams a value's formatted bytes straight into
/// a hasher, so a `Debug`/`Display` value can be folded into a fingerprint
/// without allocating an intermediate `String`.
///
/// Gated to match `frame_fingerprint`, its only constructor. Without the gate
/// a `--release` build strips the consumer and leaves the struct dead, which
/// `[lints.rust] warnings = "deny"` turns into a build failure — one that no
/// debug-profile job can see.
#[cfg(debug_assertions)]
struct HashWrite<'a, H: Hasher>(&'a mut H);

#[cfg(debug_assertions)]
impl<H: Hasher> std::fmt::Write for HashWrite<'_, H> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.write(s.as_bytes());
        Ok(())
    }
}

/// Fingerprint every input that determines the assembled chat lines + image
/// click map: the message set (role, kind, content, thinking, actions, image
/// count, timestamp, metadata), the theme identity, the content width, the
/// reasoning toggle. Nothing here depends on the clock: user rows carry no
/// timestamp, so a frame is a function of the transcript alone.
///
/// Two frames with the same fingerprint assemble byte-identical lines, so the
/// result can be memoized across frames (F31). Uses the same 64-bit-hash-keyed
/// caching the per-message #134 cache already relies on; the complex non-`Hash`
/// fields (`metadata`, `actions`) are folded in via their `Debug` form so no
/// rendered field is silently missed.
/// The frame-memo key. `content_key` identifies the transcript in O(1) (see
/// `render::chat_content_key`); this folds in the render inputs the widget
/// itself owns.
pub(crate) fn frame_key(
    content_key: u64,
    theme_seed: u64,
    content_width: u16,
    show_reasoning: bool,
) -> u64 {
    let mut h = rustc_hash::FxHasher::default();
    content_key.hash(&mut h);
    theme_seed.hash(&mut h);
    content_width.hash(&mut h);
    show_reasoning.hash(&mut h);
    h.finish()
}

/// Content key for tests and the bench rig, which render `ChatWidget` directly
/// with a bare message slice and have no `State` to read a revision from.
/// Hashing the content is O(n) but correct, which is what a test wants.
#[cfg(test)]
pub(crate) fn test_content_key(messages: &[ChatMessage]) -> u64 {
    let mut h = rustc_hash::FxHasher::default();
    messages.len().hash(&mut h);
    for msg in messages {
        msg.content.hash(&mut h);
        msg.thinking.hash(&mut h);
        std::mem::discriminant(&msg.kind).hash(&mut h);
        msg.actions.len().hash(&mut h);
    }
    h.finish()
}

/// The OLD full-content fingerprint, retained as a debug-only cross-check on
/// [`frame_key`]'s O(1) shortcut.
///
/// Rust privacy is module-scoped, so code inside `session::conversation` can
/// still touch the messages field directly and skip the revision bump that
/// `content_key` depends on. Encapsulation stops every caller outside that
/// module; this catches a mistake made inside it. Debug builds only — in
/// release it is exactly the O(transcript) cost being eliminated.
#[cfg(debug_assertions)]
pub(crate) fn frame_fingerprint(
    messages: &[ChatMessage],
    theme_seed: u64,
    content_width: u16,
    show_reasoning: bool,
    blink_on: bool,
) -> u64 {
    use std::fmt::Write as _;
    let mut h = rustc_hash::FxHasher::default();
    theme_seed.hash(&mut h);
    content_width.hash(&mut h);
    show_reasoning.hash(&mut h);
    if messages.iter().any(|m| {
        m.actions
            .iter()
            .any(|a| matches!(a.result, ActionResult::Running))
    }) {
        blink_on.hash(&mut h);
    }
    messages.len().hash(&mut h);
    for msg in messages {
        msg.content.hash(&mut h);
        msg.thinking.hash(&mut h);
        msg.timestamp.timestamp().hash(&mut h);
        msg.images
            .as_ref()
            .map_or(0, |imgs| imgs.len())
            .hash(&mut h);
        let mut hw = HashWrite(&mut h);
        let _ = write!(
            hw,
            "{:?}|{:?}|{:?}|{:?}",
            msg.role, msg.kind, msg.metadata, msg.actions
        );
    }
    h.finish()
}

impl<'a> StatefulWidget for ChatWidget<'a> {
    type State = ChatState;

    #[expect(
        clippy::too_many_lines,
        reason = "predates the lint; see .github/baselines/expect_budget.txt"
    )]
    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        // Code-block lines are tagged with this background; computed once so
        // the markdown cache key can use it.
        let code_bg = self.theme.colors.code_background.to_color();
        let theme_seed = {
            let mut h = rustc_hash::FxHasher::default();
            self.theme.colors.foreground.to_color().hash(&mut h);
            code_bg.hash(&mut h);
            self.theme.colors.header.to_color().hash(&mut h);
            h.finish()
        };

        // Content spans the full width — there is no scrollbar gutter.
        let content_width = area.width;
        let content_area = area;

        state.last_chat_area = Some((area.x, area.y, area.width, area.height));

        // F31: skip the whole per-message assembly when nothing that affects it
        // changed. The fingerprint folds in every render input, so a reused
        // frame is byte-identical to a fresh one. Scrolling and drag-selection
        // don't touch these inputs, so the common case (a static scrollback)
        // reuses the memo instead of re-parsing and re-wrapping every message.
        let frame_key = frame_key(
            self.content_key,
            theme_seed,
            content_width,
            self.show_reasoning,
        );
        // Cross-check the O(1) key against the full-content hash it replaced:
        // if the content changed, the key MUST have changed. The converse is
        // fine (a conservative revision bump only costs a memo miss).
        #[cfg(debug_assertions)]
        {
            let content_hash = frame_fingerprint(
                self.messages,
                theme_seed,
                content_width,
                self.show_reasoning,
                self.blink_on,
            );
            if let Some((last_key, last_hash)) = state.debug_key_check {
                debug_assert!(
                    last_hash == content_hash || last_key != frame_key,
                    "chat frame content changed without a new memo key — a mutation \
                     bypassed ConversationHistory::messages_mut (stale transcript risk)",
                );
            }
            state.debug_key_check = Some((frame_key, content_hash));
        }
        // TAKE the memo rather than borrowing it: owning it for the rest of the
        // render frees `state` for the scroll/selection reads below, which is
        // what used to force a full `lines.clone()` on every hit. Only the
        // VISIBLE window is cloned now (see the tail of this function), so a
        // frame costs O(viewport) instead of O(transcript) — at a 2000-message
        // scrollback that clone was ~20k `Line`s to paint 40 rows, and it
        // dominated the frame at ~98% of its cost.
        let memo = state.frame_memo.take().filter(|m| m.key == frame_key);

        let memo = if let Some(memo) = memo {
            // Memo hit: restore the click map captured alongside the lines.
            state.image_click_map = memo.click_map.clone();
            memo
        } else {
            // Memo miss: assemble fresh, then memoize for the next frame.
            let mut lines: Vec<Line<'static>> = Vec::new();

            // Clear click map for this render pass
            state.image_click_map.clear();

            for (idx, msg) in self.messages.iter().enumerate() {
                // Skip Tool messages - they're internal to the agent loop and their
                // content is already displayed inline in the assistant's action blocks
                if matches!(msg.role, MessageRole::Tool) {
                    continue;
                }

                if matches!(msg.kind, ChatMessageKind::ContextCheckpoint) {
                    if let Some(event_lines) =
                        render_context_checkpoint_event(msg, self.theme, content_width as usize)
                    {
                        lines.extend(event_lines);
                        lines.push(Line::from(""));
                    }
                    continue;
                }

                // Run summary ("Worked for … · used … tokens"): a muted gray line where
                // the spinner was — dimmer than the assistant's text (same gray as the
                // timestamp), not italic. Display-only — excluded from the model context
                // by build_chat_request, so it never accumulates as conversation.
                if matches!(msg.kind, ChatMessageKind::RunSummary) {
                    lines.push(Line::from(Span::styled(
                        format!("  {}", msg.content),
                        Style::new().fg(self.theme.colors.text_meta.to_color()),
                    )));
                    lines.push(Line::from(""));
                    continue;
                }

                // A recovery nudge is a one-shot model instruction, not user
                // content — the stitch pre-pass hides committed ones, and this
                // guard keeps a still-live one (mid-recovery) invisible too.
                // Context markers are likewise model-only (the status band is
                // the human announcement of a mode change).
                if matches!(
                    msg.kind,
                    ChatMessageKind::RecoveryNudge | ChatMessageKind::ContextMarker
                ) {
                    continue;
                }

                // System notices (warnings, agent completions, command
                // replies): muted meta text — no bullet, no timestamp. The
                // same gray as the run summary, so transcript furniture never
                // competes with the conversation.
                if matches!(msg.role, MessageRole::System) {
                    let meta = Style::new().fg(self.theme.colors.text_meta.to_color());
                    for wrapped_line in
                        wrap_text_with_indent(&msg.content, content_width as usize, 2, 2)
                    {
                        lines.push(Line::from(Span::styled(wrapped_line, meta)));
                    }
                    lines.push(Line::from(""));
                    continue;
                }

                // Auto-continue stitch, streaming half: a `Continuation`
                // extending a mergeable assistant bubble draws as that
                // bubble's tail — no fresh `●`, no blank separator — so the
                // reply reads as one message *while it streams*, not only
                // after commit (committed halves are merged upstream in
                // `stitch_committed`). An unmergeable predecessor (e.g. a
                // compaction checkpoint) falls through to a normal bubble.
                let stitch_onto_prev = matches!(msg.kind, ChatMessageKind::Continuation)
                    && self.messages[..idx]
                        .iter()
                        .rev()
                        .find(|m| !matches!(m.role, MessageRole::Tool))
                        .is_some_and(crate::render::mergeable_into);
                if stitch_onto_prev && lines.last().is_some_and(|l| line_plain_text(l).is_empty()) {
                    lines.pop();
                }

                let (role_prefix, role_color) = match msg.role {
                    MessageRole::User => (">", self.theme.colors.text_primary.to_color()),
                    MessageRole::Assistant => ("●", self.theme.colors.text_primary.to_color()),
                    MessageRole::System | MessageRole::Tool => {
                        unreachable!("System and Tool messages handled above")
                    },
                };
                // A stitched continuation keeps the 2-cell gutter but no
                // bullet: a single space prefix renders as the same margin
                // the bubble's wrapped lines already use.
                let role_prefix = if stitch_onto_prev { " " } else { role_prefix };

                if matches!(msg.role, MessageRole::Assistant) {
                    // Render thinking block if present
                    if let Some(ref thinking) = msg.thinking {
                        // Skip rendering if thinking content is empty or literal "None"
                        let thinking_trimmed = thinking.trim();
                        if thinking_trimmed.is_empty()
                            || thinking_trimmed == "None"
                            || thinking_trimmed == "none"
                        {
                            // Don't render empty/null thinking blocks
                        } else if self.show_reasoning {
                            // Add "Thinking..." header in italic and dimmed with grayed white dot
                            lines.push(Line::from(vec![
                                Span::styled(
                                    "● ",
                                    Style::new().fg(self.theme.colors.text_disabled.to_color()),
                                ),
                                Span::styled(
                                    "Thinking...",
                                    Style::new()
                                        .fg(self.theme.colors.text_secondary.to_color())
                                        .italic()
                                        .dim(),
                                ),
                            ]));

                            // Render thinking content with proper wrapping (2-space hanging indent)
                            let wrapped = wrap_text_with_indent(
                                thinking,
                                content_width as usize,
                                2, // first line indent (2 spaces)
                                2, // continuation indent (2 spaces)
                            );
                            for wrapped_line in wrapped {
                                lines.push(Line::from(Span::styled(
                                    wrapped_line,
                                    Style::new()
                                        .fg(self.theme.colors.text_secondary.to_color())
                                        .italic()
                                        .dim(),
                                )));
                            }

                            // Add blank line after thinking block
                            lines.push(Line::from(""));
                        } else if msg.content.trim().is_empty() && msg.actions.is_empty() {
                            // Reasoning is hidden and there's nothing else in this turn —
                            // skip it entirely rather than render an empty bullet. No
                            // "reasoning hidden" placeholder: /visible-reasoning controls
                            // whether the thinking shows, silently.
                            continue;
                        }
                    }

                    // Assistant prose is the bulk of the scrollback. Its wrapped,
                    // role-prefixed lines are a pure function of (content, theme,
                    // width) — exactly this key — so cache the WRAPPED output, not
                    // just the parse: a committed message is then cloned, never
                    // re-parsed or re-wrapped, each frame (#134). Theme is folded in
                    // so a theme switch can't serve stale-colored lines; width is in
                    // the key because tables wrap to the viewport.
                    let mut hasher = rustc_hash::FxHasher::default();
                    msg.content.hash(&mut hasher);
                    theme_seed.hash(&mut hasher);
                    content_width.hash(&mut hasher);
                    // A stitched continuation renders prefix-less; keep its
                    // cached lines distinct from a same-content bubble.
                    stitch_onto_prev.hash(&mut hasher);
                    let cache_key = hasher.finish();

                    let wrapped = if let Some(cached) = self.wrapped_line_cache.get(&cache_key) {
                        cached.clone()
                    } else {
                        let block = wrap_assistant_content(
                            &msg.content,
                            content_width,
                            role_prefix,
                            role_color,
                            self.theme,
                        );
                        self.wrapped_line_cache.insert(cache_key, block.clone());
                        if self.wrapped_line_cache.len()
                            > mermaid_model::constants::MARKDOWN_CACHE_MAX_ENTRIES
                        {
                            // Evict down to the cap rather than clearing the whole
                            // cache — a wholesale clear re-rendered every message each
                            // frame once a conversation exceeded the cap. Keep the
                            // entry just inserted.
                            let overflow = self.wrapped_line_cache.len()
                                - mermaid_model::constants::MARKDOWN_CACHE_MAX_ENTRIES;
                            let stale: Vec<u64> = self
                                .wrapped_line_cache
                                .keys()
                                .copied()
                                .filter(|&k| k != cache_key)
                                .take(overflow)
                                .collect();
                            for k in stale {
                                self.wrapped_line_cache.remove(&k);
                            }
                        }
                        block
                    };
                    lines.extend(wrapped);

                    // Render all actions at the end of the message
                    if !msg.actions.is_empty() {
                        // Add blank line between text content and actions
                        if !msg.content.trim().is_empty() {
                            lines.push(Line::from(""));
                        }
                        render_actions(
                            &msg.actions,
                            &mut lines,
                            self.theme,
                            content_width as usize,
                            self.blink_on,
                        );
                    }
                } else {
                    // User messages: the `>` marker, the text, and nothing else on
                    // the row. No timestamp -- Claude Code shows none, and a clock
                    // on every prompt is per-turn clutter the transcript does not
                    // need; the session picker still dates whole sessions.
                    let cleaned_content = &msg.content;
                    let role_prefix_width = role_prefix.width() + 1; // "> " = prefix + space

                    // Manually wrap the user message with hanging indent (2 spaces)
                    let wrapped = wrap_text_with_indent(
                        cleaned_content,
                        content_width as usize,
                        role_prefix_width, // reserve the prefix on the first line
                        2,                 // continuation indent
                    );

                    let band_start = lines.len();
                    for (line_idx, wrapped_line) in wrapped.iter().enumerate() {
                        if line_idx == 0 {
                            let text_content = wrapped_line.trim_start(); // Remove the indent we added
                            lines.push(Line::from(vec![
                                Span::styled(
                                    format!("{role_prefix} "),
                                    Style::new().fg(role_color).bold(),
                                ),
                                Span::raw(text_content.to_string()),
                            ]));
                        } else {
                            // Continuation lines: already have 2-space margin from wrap_text_with_indent
                            lines.push(Line::from(wrapped_line.clone()));
                        }
                    }

                    // Claude-Code-style highlight band: paint a subtle full-width
                    // background behind every line of the user's submitted prompt. The
                    // ">" marker, text, and timestamp keep their own foreground colors;
                    // only the row background is added.
                    if matches!(msg.role, MessageRole::User) {
                        let user_bg = self.theme.colors.user_message_background.to_color();
                        let cw = content_width as usize;
                        for line in &mut lines[band_start..] {
                            let used: usize = line.spans.iter().map(|s| s.content.width()).sum();
                            if used < cw {
                                line.spans.push(Span::raw(" ".repeat(cw - used)));
                            }
                            line.style = line.style.bg(user_bg);
                        }
                    }
                }

                // Show image indicators under user and assistant messages.
                // User images come from clipboard paste (`Attachment`); assistant
                // images come from tool executions that emitted `ProgressEvent::
                // Artifact` during their run — an MCP tool's image content.
                // Both land in `msg.images` as
                // base64 strings and render the same way.
                if matches!(msg.role, MessageRole::User | MessageRole::Assistant)
                    && let Some(ref images) = msg.images
                    && !images.is_empty()
                {
                    for (i, _) in images.iter().enumerate() {
                        // Record this line in the click map before pushing.
                        // `lines.len()` is usize; clamp to the u16 click-map/scroll
                        // coordinate with a saturating cast at this boundary so a
                        // scrollback past u16::MAX rows clamps instead of wrapping a
                        // stale line index into the map (F32).
                        let content_line = lines.len();
                        let image_number =
                            msg.image_numbers.as_ref().and_then(|v| v.get(i)).copied();
                        state.image_click_map.push((
                            clamp_to_u16(content_line),
                            ImageClickTarget {
                                message_index: idx,
                                image_index: i,
                                image_number,
                            },
                        ));
                        // Prefer the stable global number stored with the
                        // message; fall back to a positional index for sessions
                        // saved before image numbering (and assistant/tool
                        // images, which carry no global number).
                        let label = image_number
                            .map(|n| format!("[Image #{n}]"))
                            .unwrap_or_else(|| format!("[Image #{}]", i + 1));
                        lines.push(Line::from(vec![
                            Span::styled(
                                "  ⎿ ",
                                Style::new().fg(self.theme.colors.info.to_color()),
                            ),
                            Span::styled(
                                label,
                                Style::new().fg(self.theme.colors.info.to_color()).italic(),
                            ),
                        ]));
                    }
                }

                lines.push(Line::from(""));
            }

            // Capture the plain text of each rendered row for selection
            // extraction (before the per-frame highlight, which changes only
            // styling, not text). Recomputed only on a miss: a memo hit means
            // unchanged content, so the rows from the miss that built the memo
            // stay valid — this skips an O(total) re-collect every frame (F31).
            state.last_rendered_rows = lines.iter().map(line_plain_text).collect();

            // F31: memoize this assembly so an unchanged next frame reuses it
            // instead of re-running the loop above. Store the lines *before* the
            // selection highlight (applied per-frame below), so the cache stays
            // selection-independent. No `lines.clone()` here either — the memo
            // owns them and the visible window is cloned out of it below.
            FrameMemo {
                key: frame_key,
                lines,
                click_map: state.image_click_map.clone(),
            }
        };

        // NOTE: The response buffer is NOT rendered during streaming (buffering mode).
        // The response is buffered invisibly and only shown when generation is complete.
        // This provides a Claude Code-like experience where the complete response
        // appears instantly instead of streaming character-by-character.
        //
        // The status line shows progress: "↑ Sending..." → "↓ Streaming..." with timer

        // NOTE: `state.last_rendered_rows` (used by selection extraction) is
        // refreshed inside the memo-miss branch above, not here — a memo hit
        // keeps the rows from the miss that built it (content is unchanged on a
        // hit), so they need not be re-collected every frame (F31).

        // NOTE: Wrapping is disabled because we handle it manually with hanging
        // indents, so ONE content line is exactly one terminal row. That is what
        // makes windowing exact: rows [scroll_pos, scroll_pos + height) are the
        // only lines that can appear, so everything else is work with no pixels
        // behind it.
        //
        // `lines.len()` is usize; convert to the u16 ratatui scroll type with a
        // saturating cast so a scrollback longer than u16::MAX rows clamps the
        // scroll position instead of wrapping it (F32).
        let content_height = memo.lines.len();
        let viewport_height = area.height;

        let scroll_pos = state.get_scroll_position(clamp_to_u16(content_height), viewport_height);
        state.last_scroll_position = scroll_pos;

        // Clone ONLY the visible window. Feeding the whole transcript to
        // `Paragraph` and letting it scroll meant cloning every line to paint a
        // screenful; the window is bounded by the viewport instead.
        let first = (scroll_pos as usize).min(content_height);
        let last = first
            .saturating_add(viewport_height as usize)
            .min(content_height);
        let mut lines: Vec<Line<'static>> = memo.lines[first..last].to_vec();

        // Paint the active drag selection (reverse video over the selected
        // cells). Selection anchors are CONTENT line indices, so they are
        // rebased onto the window here — an anchor outside it simply clips.
        if let Some((a, b)) = state.selection
            && !lines.is_empty()
        {
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            let sel_style = Style::new().add_modifier(Modifier::REVERSED);
            for (offset, line) in lines.iter_mut().enumerate() {
                let content_idx = first + offset;
                if content_idx < start.0 || content_idx > end.0 {
                    continue;
                }
                let c0 = if content_idx == start.0 { start.1 } else { 0 };
                let c1 = if content_idx == end.0 {
                    end.1
                } else {
                    usize::MAX
                };
                if c1 > c0 {
                    highlight_line_cells(line, c0, c1, sel_style);
                }
            }
        }

        // Scroll is already applied by the slice, so the paragraph starts at 0.
        let paragraph = Paragraph::new(lines).block(Block::default()).scroll((0, 0));

        paragraph.render(content_area, buf);

        // Put the memo back for the next frame.
        state.frame_memo = Some(memo);
    }
}

fn render_context_checkpoint_event(
    msg: &ChatMessage,
    theme: &Theme,
    viewport_width: usize,
) -> Option<Vec<Line<'static>>> {
    if !matches!(msg.role, MessageRole::User) {
        return None;
    }

    let metadata = msg.metadata.as_ref();
    let trigger = metadata
        .and_then(|value| value.get("trigger"))
        .and_then(|value| value.as_str())
        .unwrap_or("manual");
    let before_tokens = metadata.and_then(|value| metadata_usize(value, "before_tokens"));
    let after_tokens = metadata.and_then(|value| metadata_usize(value, "after_tokens"));
    let archived_messages =
        metadata.and_then(|value| metadata_usize(value, "archived_message_count"));
    let preserved_messages =
        metadata.and_then(|value| metadata_usize(value, "preserved_message_count"));
    let duration_secs = metadata
        .and_then(|value| value.get("duration_secs"))
        .and_then(|value| value.as_f64());
    let review_status = metadata
        .and_then(|value| value.get("review_status"))
        .and_then(|value| value.as_str());
    let review_error = metadata
        .and_then(|value| value.get("review_error"))
        .and_then(|value| value.as_str());

    let action_color = theme.colors.info.to_color();
    let mut result = match (before_tokens, after_tokens) {
        (Some(before), Some(after)) => {
            format!(
                "{} -> {} tokens",
                format_compact_count(before),
                format_compact_count(after)
            )
        },
        _ => "Context compacted".to_string(),
    };

    if let Some(count) = archived_messages {
        result.push_str(&format!(
            ", archived {} {}",
            count,
            if count == 1 { "message" } else { "messages" }
        ));
    }
    if let Some(count) = preserved_messages {
        result.push_str(&format!(
            ", preserved {} {}",
            count,
            if count == 1 { "message" } else { "messages" }
        ));
    }
    if let Some(status) = review_status {
        match status {
            "reviewed" => result.push_str(", reviewed"),
            "draft_validated" => result.push_str(", validated draft"),
            _ => {},
        }
    }
    result = append_action_duration(result, duration_secs);

    let mut lines = vec![Line::from(vec![
        Span::styled("● ", Style::new().fg(action_color).bold()),
        Span::styled("Compact(", Style::new().fg(action_color).bold()),
        Span::styled(
            trigger.to_string(),
            Style::new().fg(theme.colors.text_secondary.to_color()),
        ),
        Span::styled(")", Style::new().fg(action_color).bold()),
    ])];
    lines.extend(wrap_styled_line(
        Line::from(vec![
            Span::styled("  ⎿ ", Style::new().fg(action_color)),
            Span::styled(
                result,
                Style::new().fg(theme.colors.text_secondary.to_color()),
            ),
        ]),
        viewport_width,
        4,
    ));

    if let Some(error) = review_error.filter(|error| !error.trim().is_empty()) {
        lines.extend(wrap_styled_line(
            Line::from(vec![
                Span::styled("    ", Style::new().fg(action_color)),
                Span::styled(
                    format!("review: {}", compact_inline_error(error, 180)),
                    Style::new().fg(theme.colors.warning.to_color()),
                ),
            ]),
            viewport_width,
            4,
        ));
    }

    Some(lines)
}

fn metadata_usize(value: &serde_json::Value, key: &str) -> Option<usize> {
    value
        .get(key)?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
}

fn compact_inline_error(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(3);
    let mut out: String = text.chars().take(keep).collect();
    out.push_str("...");
    out
}

/// Render actions in Claude Code style
/// Expand tab characters to spaces on 4-column tab stops.
///
/// Tabs paint as zero cells in the terminal buffer, so a line containing them
/// has a char count larger than its painted width. Any width math done by char
/// count (e.g. padding a diff line so its background bar spans the row) would
/// then come up short by one column per tab. Expanding here keeps indentation
/// visible and makes char count match painted width.
fn expand_tabs(s: &str) -> String {
    const TAB_WIDTH: usize = 4;
    if !s.contains('\t') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + TAB_WIDTH);
    let mut col = 0usize;
    for ch in s.chars() {
        if ch == '\t' {
            let n = TAB_WIDTH - (col % TAB_WIDTH);
            for _ in 0..n {
                out.push(' ');
            }
            col += n;
        } else {
            out.push(ch);
            col += UnicodeWidthChar::width(ch).unwrap_or(0);
        }
    }
    out
}

#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
fn render_actions(
    actions: &[ActionDisplay],
    lines: &mut Vec<Line>,
    theme: &Theme,
    viewport_width: usize,
    blink_on: bool,
) {
    for (action_idx, action) in actions.iter().enumerate() {
        if action_idx > 0 {
            lines.push(Line::from(""));
        }
        // An answered `ask_user_question` renders as its own block — the
        // user's answers ARE the outcome, so the transcript shows each
        // question → answer pair instead of the generic `name(target)`
        // header over a bare duration.
        if let Some(meta) = &action.metadata
            && let ToolMetadata::Questions {
                answers,
                remembered,
            } = &meta.detail
            && matches!(action.result, ActionResult::Success { .. })
        {
            render_question_answers(answers, *remembered, lines, theme, viewport_width);
            continue;
        }
        // An approved plan (`exit_plan_mode`) renders as its own block: the
        // plan body IS the outcome, shown as markdown under a header naming
        // the saved plan file.
        if let Some(meta) = &action.metadata
            && let ToolMetadata::Plan { path, body, .. } = &meta.detail
            && matches!(action.result, ActionResult::Success { .. })
        {
            render_plan_approved(path, body, lines, theme, viewport_width);
            continue;
        }
        let action_color = match action.action_type.as_str() {
            "Write" | "Update" => theme.colors.success.to_color(),
            "Delete" => theme.colors.warning.to_color(),
            _ => theme.colors.info.to_color(),
        };

        // Header: ● Type(target) — the target (a command, query, path…) wraps
        // instead of clipping at the viewport edge. Its own newlines are kept
        // as rows and overlong rows word-wrap with a hanging indent; a huge
        // target (e.g. a heredoc script) is capped so one Bash call can't
        // flood the transcript — the cap row ends in "…)" like a truncation.
        // An in-flight call's dot blinks (accent ↔ faded) as the live "this
        // one is still running" indicator; the rest of the header stays put.
        let dot_style = if matches!(action.result, ActionResult::Running) && !blink_on {
            Style::new()
                .fg(theme.colors.text_disabled.to_color())
                .bold()
        } else {
            Style::new().fg(action_color).bold()
        };
        push_action_header(
            lines,
            action,
            action_color,
            dot_style,
            theme,
            viewport_width,
        );

        match &action.result {
            // In flight: the header row (with its blinking dot) is the whole
            // display — the result elbow arrives with the outcome.
            ActionResult::Running => {},
            ActionResult::Success { .. } => {
                // Result summary from details enum
                let result_msg = match &action.details {
                    ActionDetails::FileContent { line_count, .. } => {
                        let base = format!(
                            "{} {} written",
                            line_count,
                            if *line_count == 1 { "line" } else { "lines" }
                        );
                        append_action_duration(base, action.duration_seconds)
                    },
                    ActionDetails::Diff { summary, .. } => summary.clone(),
                    ActionDetails::Preview { text, .. } => text.clone(),
                    // Success is already implied (an error renders differently),
                    // so a plain success needs no label — the header shows the
                    // action + target; the line just carries the timing.
                    ActionDetails::Simple => {
                        append_action_duration(String::new(), action.duration_seconds)
                    },
                };

                for (idx, line) in result_msg.lines().enumerate() {
                    let prefix = if idx == 0 { "  ⎿ " } else { "    " };
                    // Word-wrap the result row (4-space hanging indent) so a
                    // long summary is readable instead of clipped.
                    lines.extend(wrap_styled_line(
                        Line::from(vec![
                            Span::styled(prefix, Style::new().fg(action_color)),
                            Span::styled(
                                line.to_string(),
                                Style::new().fg(theme.colors.text_secondary.to_color()),
                            ),
                        ]),
                        viewport_width,
                        4,
                    ));
                }

                // Write: syntax-highlighted file preview
                if let ActionDetails::FileContent {
                    content,
                    line_count,
                } = &action.details
                {
                    let preview_lines: Vec<&str> = content.lines().take(10).collect();
                    if !preview_lines.is_empty() {
                        lines.push(Line::from(vec![Span::styled(
                            "    ",
                            Style::new().fg(action_color),
                        )]));

                        let preview_content = preview_lines.join("\n");
                        let mut parsed = parse_markdown(
                            &format!("```\n{preview_content}\n```"),
                            theme,
                            viewport_width.saturating_sub(4),
                        );
                        for parsed_line in parsed.iter_mut() {
                            let mut new_spans =
                                vec![Span::styled("    ", Style::new().fg(action_color))];
                            new_spans.append(&mut parsed_line.line.spans);
                            parsed_line.line.spans = new_spans;
                        }
                        // Hard-wrap (not word-wrap) so code indentation and
                        // alignment survive; overlong rows continue with a
                        // 6-space hanging indent instead of clipping.
                        lines.extend(
                            parsed
                                .into_iter()
                                .flat_map(|ml| wrap_preformatted(ml.line, viewport_width, 6)),
                        );

                        if *line_count > 10 {
                            lines.push(Line::from(vec![
                                Span::styled("    ", Style::new().fg(action_color)),
                                Span::styled(
                                    format!("... ({} more lines)", line_count - 10),
                                    Style::new()
                                        .fg(theme.colors.text_disabled.to_color())
                                        .italic(),
                                ),
                            ]));
                        }
                    }
                }

                // Edit: color-coded diff
                if let ActionDetails::Diff { diff, .. } = &action.details {
                    let diff_lines: Vec<&str> = diff.lines().collect();
                    let display_lines: Vec<&str> = diff_lines.iter().take(80).copied().collect();

                    if !display_lines.is_empty() {
                        let removed_bg = theme.colors.diff_removed_bg.to_color();
                        let added_bg = theme.colors.diff_added_bg.to_color();

                        for diff_line in &display_lines {
                            // Expand tabs first: the TUI paints a tab as zero
                            // cells, so a tab-bearing line's char count exceeds
                            // its painted width and the char-count pad below
                            // would leave the background bar short — a ragged
                            // "staircase" down the right edge. Expanding also
                            // makes tab indentation actually visible.
                            let diff_line = expand_tabs(diff_line);
                            // Delegate the producer-format awareness to
                            // `parse_diff_line`, which lives next to the
                            // marker constants and stays in lockstep with
                            // any future format change.
                            match parse_diff_line(&diff_line) {
                                DiffLineKind::Removed => {
                                    push_wrapped_diff_rows(
                                        lines,
                                        format!("    {diff_line}"),
                                        Style::new()
                                            .fg(theme.colors.error.to_color())
                                            .bg(removed_bg),
                                        viewport_width,
                                    );
                                },
                                DiffLineKind::Added => {
                                    push_wrapped_diff_rows(
                                        lines,
                                        format!("    {diff_line}"),
                                        Style::new()
                                            .fg(theme.colors.success.to_color())
                                            .bg(added_bg),
                                        viewport_width,
                                    );
                                },
                                DiffLineKind::Context => {
                                    // Hard-wrap like the colored rows so an
                                    // overlong context line isn't clipped.
                                    lines.extend(wrap_preformatted(
                                        Line::from(vec![
                                            Span::styled("    ", Style::new().fg(action_color)),
                                            Span::styled(
                                                diff_line,
                                                Style::new()
                                                    .fg(theme.colors.text_secondary.to_color()),
                                            ),
                                        ]),
                                        viewport_width,
                                        6,
                                    ));
                                },
                            }
                        }

                        let remaining = diff_lines.len().saturating_sub(display_lines.len());
                        if remaining > 0 {
                            lines.push(Line::from(vec![
                                Span::styled("    ", Style::new().fg(action_color)),
                                Span::styled(
                                    format!("... ({remaining} more lines)"),
                                    Style::new()
                                        .fg(theme.colors.text_disabled.to_color())
                                        .italic(),
                                ),
                            ]));
                        }
                    }
                }
            },
            ActionResult::Error { error } => {
                let error =
                    append_action_duration(format!("Error: {error}"), action.duration_seconds);
                // Word-wrap so the full error body (an HTTP error JSON can run
                // hundreds of cells) is readable instead of clipped at the
                // viewport edge. Multi-line errors keep their own rows.
                for (idx, err_line) in error.lines().enumerate() {
                    let prefix = if idx == 0 { "  ⎿ " } else { "    " };
                    lines.extend(wrap_styled_line(
                        Line::from(vec![
                            Span::styled(prefix, Style::new().fg(theme.colors.error.to_color())),
                            Span::styled(
                                err_line.to_string(),
                                Style::new().fg(theme.colors.error.to_color()),
                            ),
                        ]),
                        viewport_width,
                        4,
                    ));
                }
            },
        }
    }
}

/// Record of an approved plan (`exit_plan_mode`): a header bullet naming the
/// plan file, then the plan body rendered as markdown under the elbow gutter
/// — the transcript keeps the exact text the user approved.
fn render_plan_approved(
    path: &str,
    body: &str,
    lines: &mut Vec<Line>,
    theme: &Theme,
    viewport_width: usize,
) {
    lines.push(Line::from(Span::styled(
        format!("● User approved the plan — {path}"),
        Style::new().fg(theme.colors.success.to_color()),
    )));
    let gutter_style = Style::new().fg(theme.colors.text_secondary.to_color());
    // The 4-cell gutter comes off the markdown wrap budget, matching the
    // question→answer block above.
    let parsed = parse_markdown(body, theme, viewport_width.saturating_sub(4));
    let mut first_row = true;
    for mut parsed_line in parsed {
        let gutter = if first_row { "  ⎿ " } else { "    " };
        first_row = false;
        let mut spans = vec![Span::styled(gutter, gutter_style)];
        spans.append(&mut parsed_line.line.spans);
        lines.push(Line::from(spans));
    }
}

/// Claude-Code-style record of an answered `ask_user_question` call: a plain
/// header bullet plus one `· question → answer` line per question, so the
/// transcript preserves what the user chose (not just how long it took).
fn render_question_answers(
    answers: &[QuestionAnswer],
    remembered: bool,
    lines: &mut Vec<Line>,
    theme: &Theme,
    viewport_width: usize,
) {
    let header = if remembered {
        "User answered the model's questions (remembered):"
    } else {
        "User answered the model's questions:"
    };
    lines.push(Line::from(Span::styled(
        format!("● {header}"),
        Style::new().fg(theme.colors.text_primary.to_color()),
    )));

    let gutter_style = Style::new().fg(theme.colors.text_secondary.to_color());
    let text_style = Style::new().fg(theme.colors.text_secondary.to_color());
    let note_style = Style::new()
        .fg(theme.colors.text_disabled.to_color())
        .italic();
    // The 4-cell gutter ("  ⎿ " on the first row, "    " after) comes off the
    // wrap budget; continuations hang 2 cells so wrapped text aligns under
    // the question, not the `·`.
    let wrap_width = viewport_width.saturating_sub(4);
    let mut first_row = true;
    for answer in answers {
        let value = if answer.selected.is_empty() {
            "(no selection)".to_string()
        } else {
            answer.selected.join(", ")
        };
        let entry = format!("· {} → {}", answer.question, value);
        let mut rows: Vec<(String, Style)> = wrap_text_with_indent(&entry, wrap_width, 0, 2)
            .into_iter()
            .map(|row| (row, text_style))
            .collect();
        if let Some(note) = &answer.note {
            rows.extend(
                wrap_text_with_indent(&format!("(note: {note})"), wrap_width, 2, 4)
                    .into_iter()
                    .map(|row| (row, note_style)),
            );
        }
        for (row, style) in rows {
            let gutter = if first_row { "  ⎿ " } else { "    " };
            first_row = false;
            lines.push(Line::from(vec![
                Span::styled(gutter, gutter_style),
                Span::styled(row, style),
            ]));
        }
    }
}

/// Cap on wrapped action-header rows: a long target (a Bash heredoc, a huge
/// query) wraps for readability, but past this many rows it truncates with
/// "…)" so a single tool call can't flood the transcript.
const MAX_ACTION_HEADER_ROWS: usize = 4;

/// Push the "● Type(target)" action header, wrapping the target across rows
/// instead of letting an over-wide one clip at the viewport edge.
///
/// The target's own newlines are preserved as row breaks; overlong rows
/// word-wrap with a 4-space hanging indent (an unbroken token hard-breaks).
/// Two cells are reserved so the closing ")" — and the "…" a capped header
/// gains — never overflow the last row.
fn push_action_header(
    lines: &mut Vec<Line>,
    action: &ActionDisplay,
    action_color: Color,
    dot_style: Style,
    theme: &Theme,
    viewport_width: usize,
) {
    let bold = Style::new().fg(action_color).bold();
    let secondary = Style::new().fg(theme.colors.text_secondary.to_color());
    if action.target.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("● ", dot_style),
            Span::styled(format!("{}()", action.action_type), bold),
        ]));
        return;
    }

    let open = format!("{}(", action.action_type);
    // The first row's indent stands in for the 2-cell "● " plus the opening
    // "Type(" so wrapping accounts for them; it is stripped and replaced with
    // the real styled spans below.
    let first_indent = 2 + open.width();
    let wrap_width = viewport_width.saturating_sub(2).max(first_indent + 1);
    let mut rows = wrap_text_with_indent(&action.target, wrap_width, first_indent, 4);
    let truncated = rows.len() > MAX_ACTION_HEADER_ROWS;
    rows.truncate(MAX_ACTION_HEADER_ROWS);

    let last = rows.len().saturating_sub(1);
    for (i, row) in rows.into_iter().enumerate() {
        let mut spans = if i == 0 {
            vec![
                Span::styled("● ", dot_style),
                Span::styled(open.clone(), bold),
                Span::styled(row.trim_start().to_string(), secondary),
            ]
        } else {
            vec![Span::styled(row, secondary)]
        };
        if i == last {
            if truncated {
                spans.push(Span::styled(
                    "…",
                    Style::new().fg(theme.colors.text_disabled.to_color()),
                ));
            }
            spans.push(Span::styled(")", bold));
        }
        lines.push(Line::from(spans));
    }
}

/// Push one colored diff row, hard-wrapped at the viewport width and padded so
/// every produced row carries the full-width background bar (no unfilled
/// column on a diff row — the "staircase" invariant).
fn push_wrapped_diff_rows(lines: &mut Vec<Line>, text: String, style: Style, width: usize) {
    for row in wrap_preformatted(Line::from(Span::raw(text)), width, 6) {
        let padded = pad_to_cells(&line_plain_text(&row), width);
        lines.push(Line::from(Span::styled(padded, style)));
    }
}

fn append_action_duration(mut text: String, duration_seconds: Option<f64>) -> String {
    if let Some(seconds) = duration_seconds {
        // An empty base (a plain success with no detail) becomes just
        // "took Xms" — no leading comma.
        if !text.is_empty() {
            text.push_str(", ");
        }
        text.push_str("took ");
        text.push_str(&format_action_duration(seconds));
    }
    text
}

fn format_action_duration(seconds: f64) -> String {
    if seconds < 1.0 {
        format!("{}ms", (seconds * 1000.0).round().max(1.0) as u64)
    } else if seconds < 10.0 {
        format!("{seconds:.1}s")
    } else {
        format!("{}s", seconds.round() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_answers_render_as_question_arrow_answer_block() {
        use mermaid_domain::{QuestionAnswer, ToolMetadata, ToolRunMetadata};

        let theme = Theme::dark();
        let answers = vec![
            QuestionAnswer {
                header: "Snack".to_string(),
                question: "Which snack fuels your next coding session?".to_string(),
                selected: vec!["Coffee (Recommended)".to_string()],
                note: None,
            },
            QuestionAnswer {
                header: "Powers".to_string(),
                question: "Which superpowers would you take?".to_string(),
                selected: vec![
                    "Read any codebase instantly".to_string(),
                    "Bugs reproduce on demand".to_string(),
                ],
                note: Some("only on weekdays".to_string()),
            },
        ];
        let action = ActionDisplay {
            action_type: "ask_user_question".to_string(),
            target: String::new(),
            result: ActionResult::Success {
                output: String::new(),
                images: None,
            },
            details: ActionDetails::Simple,
            duration_seconds: Some(93.0),
            metadata: Some(ToolRunMetadata {
                detail: ToolMetadata::Questions {
                    answers,
                    remembered: false,
                },
                ..Default::default()
            }),
        };

        let mut lines: Vec<Line> = Vec::new();
        render_actions(&[action], &mut lines, &theme, 120, true);
        let rows: Vec<String> = lines.iter().map(line_plain_text).collect();
        let all = rows.join("\n");

        assert_eq!(rows[0], "● User answered the model's questions:");
        assert!(
            rows[1].starts_with("  ⎿ · Which snack fuels your next coding session? → Coffee"),
            "got {:?}",
            rows[1]
        );
        assert!(
            all.contains(
                "· Which superpowers would you take? → Read any codebase instantly, \
                 Bugs reproduce on demand"
            ),
            "got {all}"
        );
        assert!(all.contains("(note: only on weekdays)"), "got {all}");
        // The generic `name()` header and duration line are replaced entirely.
        assert!(!all.contains("ask_user_question("), "got {all}");
        assert!(!all.contains("took"), "got {all}");
    }

    #[test]
    fn diff_background_fills_full_width_with_tabs() {
        // Regression: tab characters paint as zero cells, so char-count padding
        // left the red/green diff bar short by one column per tab — a ragged
        // "staircase" down the right edge. After expand_tabs, every column of a
        // diff row must carry the background.
        use mermaid_model::diff::{DIFF_ADDED_MARKER, DIFF_REMOVED_MARKER};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::dark();
        let added_bg = theme.colors.diff_added_bg.to_color();
        let removed_bg = theme.colors.diff_removed_bg.to_color();
        // Lines at increasing tab depth — the exact shape that staircased.
        let diff = format!(
            "  62{DIFF_REMOVED_MARKER}\tconst out = [];\n  63{DIFF_ADDED_MARKER}\t\tlet fixed = false;\n  64{DIFF_ADDED_MARKER}\t\t\tdeeplyNested();"
        );
        let action = ActionDisplay {
            action_type: "Update".to_string(),
            target: "engine.ts".to_string(),
            result: ActionResult::Success {
                output: String::new(),
                images: None,
            },
            details: ActionDetails::Diff {
                summary: "ok".to_string(),
                diff,
            },
            duration_seconds: Some(0.3),
            metadata: None,
        };

        let width: u16 = 60;
        let mut lines: Vec<Line> = Vec::new();
        render_actions(&[action], &mut lines, &theme, width as usize, true);
        let h = lines.len() as u16;
        let backend = TestBackend::new(width, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            Paragraph::new(lines).render(Rect::new(0, 0, width, h), f.buffer_mut());
        })
        .unwrap();
        let buf = term.backend().buffer();

        for y in 0..h {
            let is_diff_row = (0..width).any(|x| {
                let bg = buf[(x, y)].bg;
                bg == added_bg || bg == removed_bg
            });
            if !is_diff_row {
                continue;
            }
            for x in 0..width {
                let bg = buf[(x, y)].bg;
                assert!(
                    bg == added_bg || bg == removed_bg,
                    "diff background must fill the whole row, but column {x} of row {y} is unfilled (staircase)"
                );
            }
        }
    }

    /// Every rendered action row must fit the viewport width — overlong
    /// headers, results, and errors wrap instead of clipping at the edge.
    fn assert_rows_fit(lines: &[Line], width: usize) {
        for (i, line) in lines.iter().enumerate() {
            let w: usize = line.spans.iter().map(|s| s.content.width()).sum();
            assert!(
                w <= width,
                "row {i} is {w} cells wide, exceeding the {width}-cell viewport: {:?}",
                line_plain_text(line)
            );
        }
    }

    #[test]
    fn action_header_and_error_wrap_instead_of_clipping() {
        // Regression: a long Bash command in the header and a long HTTP error
        // body in the result were painted as single over-wide rows and clipped
        // at the viewport edge instead of wrapping.
        let theme = Theme::dark();
        let action = ActionDisplay {
            action_type: "Error".to_string(),
            target: "Backend error".to_string(),
            result: ActionResult::Error {
                error: r#"HTTP error 404: {"error":{"code":"model_not_found","message":"The requested model was not found.","param":null,"type":"invalid_request_error"}}"#.to_string(),
            },
            details: ActionDetails::Simple,
            duration_seconds: None,
            metadata: None,
        };

        let width = 60usize;
        let mut lines: Vec<Line> = Vec::new();
        render_actions(&[action], &mut lines, &theme, width, true);

        assert_rows_fit(&lines, width);
        let rendered = lines
            .iter()
            .map(line_plain_text)
            .collect::<Vec<_>>()
            .join("\n");
        // The full error body must survive the wrap (word boundaries may move,
        // so check the tail token that clipping used to cut off).
        assert!(rendered.contains("invalid_request_error"));
        assert!(
            lines.len() > 2,
            "a 140-cell error at width 60 must span multiple rows"
        );
    }

    #[test]
    fn action_header_wraps_long_command_and_keeps_closing_paren() {
        let theme = Theme::dark();
        let action = ActionDisplay {
            action_type: "Bash".to_string(),
            target: "python3 -c 'print(1)' && echo a-very-long-command-line \
                     that keeps going well past the sixty cell viewport edge"
                .to_string(),
            result: ActionResult::Success {
                output: String::new(),
                images: None,
            },
            details: ActionDetails::Simple,
            duration_seconds: Some(0.1),
            metadata: None,
        };

        let width = 60usize;
        let mut lines: Vec<Line> = Vec::new();
        render_actions(&[action], &mut lines, &theme, width, true);

        assert_rows_fit(&lines, width);
        let rows: Vec<String> = lines.iter().map(line_plain_text).collect();
        assert!(rows[0].starts_with("● Bash("));
        assert!(
            rows.len() >= 2,
            "the long command must wrap the header across rows"
        );
        let last_target_row = rows
            .iter()
            .rfind(|r| r.trim_end().ends_with(')'))
            .expect("wrapped header must still close its paren");
        assert!(last_target_row.trim_end().ends_with(')'));
    }

    #[test]
    fn action_header_caps_rows_and_marks_truncation() {
        // A heredoc-sized target must not flood the transcript: the header
        // caps at MAX_ACTION_HEADER_ROWS and the last row signals "…)".
        let theme = Theme::dark();
        let action = ActionDisplay {
            action_type: "Bash".to_string(),
            target: "word ".repeat(400),
            result: ActionResult::Success {
                output: String::new(),
                images: None,
            },
            details: ActionDetails::Simple,
            duration_seconds: None,
            metadata: None,
        };

        let width = 60usize;
        let mut lines: Vec<Line> = Vec::new();
        render_actions(&[action], &mut lines, &theme, width, true);

        assert_rows_fit(&lines, width);
        let header_rows: Vec<String> = lines
            .iter()
            .map(line_plain_text)
            .take_while(|r| !r.trim_start().starts_with('⎿'))
            .collect();
        assert_eq!(
            header_rows.len(),
            MAX_ACTION_HEADER_ROWS,
            "header must cap at MAX_ACTION_HEADER_ROWS rows"
        );
        assert!(
            header_rows.last().unwrap().trim_end().ends_with("…)"),
            "capped header must end with …) — got {:?}",
            header_rows.last().unwrap()
        );
    }

    #[test]
    fn action_header_preserves_multiline_command_rows() {
        // A multi-line command (heredoc-style) keeps its own line breaks in
        // the header instead of the old behavior where ratatui dropped the
        // newlines and glued fragments together ("'PY'from PIL import…").
        let theme = Theme::dark();
        let action = ActionDisplay {
            action_type: "Bash".to_string(),
            target: "python3 - << 'PY'\nfrom PIL import Image\nPY".to_string(),
            result: ActionResult::Success {
                output: String::new(),
                images: None,
            },
            details: ActionDetails::Simple,
            duration_seconds: None,
            metadata: None,
        };

        let mut lines: Vec<Line> = Vec::new();
        render_actions(&[action], &mut lines, &theme, 80, true);

        let rows: Vec<String> = lines.iter().map(line_plain_text).collect();
        assert!(rows[0].contains("python3 - << 'PY'"));
        assert!(rows[1].contains("from PIL import Image"));
        assert!(!rows[0].contains("'PY'from"), "newline must not be dropped");
    }

    #[test]
    fn action_result_summary_wraps_instead_of_clipping() {
        let theme = Theme::dark();
        let action = ActionDisplay {
            action_type: "Tasks".to_string(),
            target: "update 3 steps".to_string(),
            result: ActionResult::Success {
                output: String::new(),
                images: None,
            },
            details: ActionDetails::Preview {
                text: "Tasks 5/6 · User chose SKIP for domain/phone/address - \
                       placeholders kept intentionally until real data available. \
                       Task 2 and 6 deferred., to revisit later"
                    .to_string(),
                line_count: None,
            },
            duration_seconds: None,
            metadata: None,
        };

        let width = 60usize;
        let mut lines: Vec<Line> = Vec::new();
        render_actions(&[action], &mut lines, &theme, width, true);

        assert_rows_fit(&lines, width);
        let rendered = lines
            .iter()
            .map(line_plain_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("revisit later"),
            "the summary's tail must survive the wrap instead of being clipped"
        );
    }

    #[test]
    fn wrapped_line_cache_hit_matches_cache_miss() {
        // #134: caching the WRAPPED assistant lines must be byte-for-byte
        // identical to wrapping fresh. Render the same messages through a shared
        // cache — first call misses (populates), second hits — and assert the
        // two frame buffers are equal; then prove a cold cache renders the same
        // frame as the warm one. Assistant-only messages keep the frame free of
        // the time-relative user timestamp, so nothing here is clock-dependent.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::dark();
        let messages = vec![
            ChatMessage::assistant(
                "# Heading\n\nSome **bold** prose long enough that it has to wrap \
                 across this narrow viewport more than once.\n\n\
                 - a list item that also keeps going past the edge so it wraps too\n\
                 - second item\n\n```rust\nfn a_very_long_preformatted_code_line_that_overflows() {}\n```",
            ),
            ChatMessage::assistant("Short follow-up paragraph."),
        ];

        let (width, height): (u16, u16) = (40, 40);
        let render_once = |cache: &mut FxHashMap<u64, Vec<Line<'static>>>| {
            let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut state = ChatState::new();
            term.draw(|f| {
                let widget = ChatWidget {
                    messages: &messages,
                    content_key: test_content_key(&messages),
                    theme: &theme,
                    wrapped_line_cache: cache,
                    show_reasoning: true,
                    blink_on: true,
                };
                f.render_stateful_widget(widget, Rect::new(0, 0, width, height), &mut state);
            })
            .unwrap();
            term.backend().buffer().clone()
        };

        let mut shared = FxHashMap::default();
        let miss = render_once(&mut shared);
        assert!(!shared.is_empty(), "first render must populate the cache");
        let hit = render_once(&mut shared);
        assert_eq!(miss, hit, "cache hit must render identically to cache miss");

        let mut cold_cache = FxHashMap::default();
        let cold = render_once(&mut cold_cache);
        assert_eq!(hit, cold, "warm-cache frame must equal a cold-cache frame");
    }

    #[test]
    fn system_notice_renders_as_dim_meta_text_without_bullet_or_timestamp() {
        // System notices are transcript furniture, not conversation: they must
        // render as indented muted-gray text — no role bullet, no right-aligned
        // timestamp (both belonged to the old user-layout share).
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::dark();
        let messages = vec![ChatMessage::system(
            "Heads up: this model reports no vision capability",
        )];
        let (width, height): (u16, u16) = (60, 10);
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut state = ChatState::new();
        let mut cache = FxHashMap::default();
        term.draw(|f| {
            let widget = ChatWidget {
                messages: &messages,
                content_key: test_content_key(&messages),
                theme: &theme,
                wrapped_line_cache: &mut cache,
                show_reasoning: true,
                blink_on: true,
            };
            f.render_stateful_widget(widget, Rect::new(0, 0, width, height), &mut state);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let rows: Vec<String> = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        let all = rows.join("\n");
        assert!(
            !all.contains('●'),
            "no role bullet on system notices: {all}"
        );
        assert!(
            !all.contains("Today at"),
            "no timestamp on system notices: {all}"
        );
        let row = rows
            .iter()
            .position(|r| r.contains("Heads up"))
            .expect("notice rendered");
        assert!(
            rows[row].starts_with("  Heads up"),
            "2-space indent, nothing in the gutter: {:?}",
            rows[row]
        );
        let col = rows[row].find("Heads up").unwrap(); // ASCII row: byte == cell
        assert_eq!(
            buf[(col as u16, row as u16)].fg,
            theme.colors.text_meta.to_color(),
            "notice text uses the muted meta gray"
        );
    }

    #[test]
    fn byte_at_cell_clamps_and_respects_cjk() {
        assert_eq!(byte_at_cell("hello", 0), 0);
        assert_eq!(byte_at_cell("hello", 3), 3);
        assert_eq!(byte_at_cell("hello", 99), 5); // clamp past end
        // "你好" = 2 chars, 3 bytes each, 2 cells each.
        assert_eq!(byte_at_cell("你好", 0), 0);
        assert_eq!(byte_at_cell("你好", 2), 3); // after first wide char
        // A cell index that lands mid-glyph keeps the glyph whole (rounds up).
        assert_eq!(byte_at_cell("你好", 1), 3);
    }

    #[test]
    fn slice_by_cells_extracts_display_range() {
        assert_eq!(slice_by_cells("hello world", 0, 5), "hello");
        assert_eq!(slice_by_cells("hello world", 6, 11), "world");
        assert_eq!(slice_by_cells("你好world", 2, 7), "好wor");
    }

    #[test]
    fn pad_to_cells_fills_to_display_width() {
        assert_eq!(pad_to_cells("ab", 5), "ab   ");
        // "你好" = 4 display cells; pad to 6 → exactly 2 trailing spaces (#101).
        assert_eq!(pad_to_cells("你好", 6), "你好  ");
        // Already wide enough → unchanged (never truncates).
        assert_eq!(pad_to_cells("你好", 3), "你好");
        assert_eq!(pad_to_cells("", 0), "");
    }

    #[test]
    fn wrap_preformatted_hard_wraps_preserving_spaces() {
        // 18 cells, wraps at 10. Spaces are preserved (not collapsed) and the
        // leading indentation survives on the first row.
        let line = Line::from(vec![Span::raw("    aaaa bbbb cccc")]);
        let wrapped = wrap_preformatted(line, 10, 2);
        assert!(wrapped.len() >= 2, "wide line should wrap to multiple rows");
        let first: String = wrapped[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            first.starts_with("    aaaa"),
            "indentation must be preserved, got {first:?}"
        );
        let second: String = wrapped[1]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            second.starts_with("  "),
            "continuation should get the hanging indent, got {second:?}"
        );
    }

    #[test]
    fn wrap_preformatted_short_line_unchanged() {
        let line = Line::from(vec![Span::raw("    short")]);
        let wrapped = wrap_preformatted(line, 40, 2);
        assert_eq!(wrapped.len(), 1);
        let text: String = wrapped[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text, "    short");
    }

    /// Build a `ChatState` whose last frame rendered `rows`, with a selection
    /// already mapped to content coords, so `selected_text` can be tested
    /// without a real terminal.
    fn state_with_rows(rows: &[&str], sel: ((usize, usize), (usize, usize))) -> ChatState {
        let mut st = ChatState::new();
        st.last_rendered_rows = rows.iter().map(|r| r.to_string()).collect();
        st.selection = Some(sel);
        st
    }

    #[test]
    fn selected_text_single_line() {
        let st = state_with_rows(&["> hello world"], ((0, 2), (0, 7)));
        assert_eq!(st.selected_text().as_deref(), Some("hello"));
    }

    #[test]
    fn selected_text_spans_multiple_rows() {
        let st = state_with_rows(&["> first line", "  second line"], ((0, 2), (1, 8)));
        // The continuation row's "  " margin is stripped so copied text is
        // clean (the start row was sliced from the click column past "> ").
        assert_eq!(st.selected_text().as_deref(), Some("first line\nsecond"));
    }

    #[test]
    fn selected_text_strips_margin_but_keeps_code_indentation() {
        // Rendered rows: 2-cell margin + the code's own indentation. Selecting
        // from column 0 must drop only the 2-cell margin, not the code indent.
        let st = state_with_rows(
            &["  fn main() {", "      let x = 1;", "  }"],
            ((0, 0), (2, 3)),
        );
        assert_eq!(
            st.selected_text().as_deref(),
            Some("fn main() {\n    let x = 1;\n}")
        );
    }

    #[test]
    fn selected_text_normalizes_reversed_drag() {
        // Dragging bottom-up / right-to-left yields the same text.
        let st = state_with_rows(&["> hello world"], ((0, 7), (0, 2)));
        assert_eq!(st.selected_text().as_deref(), Some("hello"));
    }

    #[test]
    fn selected_text_empty_selection_is_none() {
        // A plain click (anchor == cursor) selects nothing.
        let st = state_with_rows(&["> hello"], ((0, 3), (0, 3)));
        assert_eq!(st.selected_text(), None);
    }

    #[test]
    fn highlight_line_cells_splits_spans_on_selection() {
        let mut line = Line::from(vec![Span::raw("abcdef")]);
        highlight_line_cells(
            &mut line,
            2,
            4,
            Style::new().add_modifier(Modifier::REVERSED),
        );
        // Split into "ab" | "cd"(reversed) | "ef".
        let texts: Vec<String> = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(texts, vec!["ab", "cd", "ef"]);
        assert!(
            line.spans[1]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            !line.spans[0]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn context_checkpoint_renders_as_compact_event() {
        let mut msg = ChatMessage::user("full checkpoint summary hidden from the chat log");
        msg.kind = ChatMessageKind::ContextCheckpoint;
        msg.metadata = Some(serde_json::json!({
            "trigger": "manual",
            "before_tokens": 43_800,
            "after_tokens": 9_200,
            "archived_message_count": 18,
            "preserved_message_count": 4,
            "duration_secs": 2.4,
            "review_status": "reviewed",
        }));

        let lines =
            render_context_checkpoint_event(&msg, &Theme::dark(), 120).expect("event lines");
        let rendered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("Compact(manual)"));
        assert!(rendered.contains("43.8k -> 9.2k tokens"));
        assert!(rendered.contains("archived 18 messages"));
        assert!(rendered.contains("preserved 4 messages"));
        assert!(rendered.contains("reviewed"));
        assert!(!rendered.contains("full checkpoint summary"));
    }

    #[test]
    fn context_checkpoint_renders_validated_draft() {
        let mut msg = ChatMessage::user("full checkpoint summary hidden from the chat log");
        msg.kind = ChatMessageKind::ContextCheckpoint;
        msg.metadata = Some(serde_json::json!({
            "trigger": "auto_threshold",
            "before_tokens": 43_800,
            "after_tokens": 9_200,
            "archived_message_count": 18,
            "preserved_message_count": 4,
            "duration_secs": 2.4,
            "review_status": "draft_validated",
            "review_error": "provider overloaded",
        }));

        let lines =
            render_context_checkpoint_event(&msg, &Theme::dark(), 120).expect("event lines");
        let rendered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("Compact(auto_threshold)"));
        assert!(rendered.contains("validated draft"));
        assert!(rendered.contains("review: provider overloaded"));
    }

    #[test]
    fn clamp_to_u16_saturates_past_u16_max() {
        // F32: line counters past u16::MAX must clamp to the last addressable
        // row, never wrap modulo 65536 (which a plain `as u16` would do).
        assert_eq!(clamp_to_u16(0), 0);
        assert_eq!(clamp_to_u16(65_535), u16::MAX);
        assert_eq!(clamp_to_u16(65_536), u16::MAX);
        assert_eq!(clamp_to_u16(1_000_000), u16::MAX);
    }

    #[test]
    fn frame_memo_hit_matches_miss() {
        // F31: memoizing the assembled frame must be byte-for-byte identical to
        // re-assembling it. Render the SAME state twice — the first render
        // populates the frame memo, the second reuses it — and assert the
        // buffers are equal. Assistant-only messages keep the frame free of the
        // clock-relative user timestamp, so nothing here is time-dependent.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::dark();
        let messages = vec![
            ChatMessage::assistant(
                "# Heading\n\nSome **bold** prose long enough that it wraps across \
                 this narrow viewport more than once.\n\n- a list item that also \
                 runs past the edge so it wraps\n- second item",
            ),
            ChatMessage::assistant("Short follow-up."),
        ];

        let (width, height): (u16, u16) = (34, 30);
        let mut cache = FxHashMap::default();
        let mut state = ChatState::new();

        let render = |state: &mut ChatState, cache: &mut FxHashMap<u64, Vec<Line<'static>>>| {
            let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
            term.draw(|f| {
                let widget = ChatWidget {
                    messages: &messages,
                    content_key: test_content_key(&messages),
                    theme: &theme,
                    wrapped_line_cache: cache,
                    show_reasoning: true,
                    blink_on: true,
                };
                f.render_stateful_widget(widget, Rect::new(0, 0, width, height), state);
            })
            .unwrap();
            term.backend().buffer().clone()
        };

        let miss = render(&mut state, &mut cache);
        assert!(
            state.frame_memo.is_some(),
            "first render must populate the frame memo"
        );
        let hit = render(&mut state, &mut cache);
        assert_eq!(
            miss, hit,
            "frame-memo hit must render identically to the miss"
        );
        // The rows used for selection extraction are only re-collected on a
        // miss; assert the hit path left them intact (not cleared/stale) so
        // copy/selection still works on a reused frame (F31).
        assert!(
            !state.last_rendered_rows.is_empty(),
            "memo hit must preserve last_rendered_rows from the miss"
        );
    }

    #[test]
    fn append_action_duration_handles_empty_base() {
        // A plain success with no detail (e.g. the Delete line) → just "took Xms",
        // no leading comma.
        assert_eq!(
            append_action_duration(String::new(), Some(0.035)),
            "took 35ms"
        );
        // A detail line keeps its text before the timing.
        assert_eq!(
            append_action_duration("3 lines read".to_string(), Some(1.25)),
            "3 lines read, took 1.2s"
        );
        // No duration → text unchanged (empty stays empty → renders no line).
        assert_eq!(append_action_duration(String::new(), None), "");
    }
}
