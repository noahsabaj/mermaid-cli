//! Input state management
//!
//! User input buffer, cursor handling, and input history navigation.

use std::collections::VecDeque;

/// Maximum entries kept in the input history deque. Matches the cap used
/// by ConversationHistory::add_to_input_history so persisted and in-memory
/// history stay in the same shape.
const INPUT_HISTORY_CAP: usize = 100;

/// Input state - user input buffer, cursor, and history
///
/// All cursor positions are byte offsets that are guaranteed to sit on
/// UTF-8 char boundaries. Methods navigate by whole characters, not
/// raw bytes, so multi-byte input (emoji, CJK, accented chars) is safe.
pub struct InputBuffer {
    /// User input buffer
    pub content: String,
    /// Cursor position as a **byte offset** (always on a char boundary)
    pub cursor_position: usize,
    /// Input history for arrow key navigation (persisted across sessions)
    pub history: VecDeque<String>,
    /// Current position in history (None = editing current input, Some(i) = viewing history[i])
    pub history_index: Option<usize>,
    /// Saved input when navigating away from current draft
    pub history_buffer: String,
}

impl InputBuffer {
    /// Create a new empty input buffer
    pub fn new() -> Self {
        Self {
            content: String::new(),
            cursor_position: 0,
            history: VecDeque::new(),
            history_index: None,
            history_buffer: String::new(),
        }
    }

    /// Load persisted input history (from a resumed conversation)
    pub fn load_history(&mut self, history: VecDeque<String>) {
        self.history = history;
    }

    /// Append an input to history with dedup + cap, mirroring
    /// ConversationHistory::add_to_input_history.
    ///
    /// - Empty/whitespace inputs are ignored.
    /// - Consecutive duplicates of the last entry are skipped.
    /// - Oldest entry is evicted when the cap is reached.
    pub fn add_to_history(&mut self, input: String) {
        if input.trim().is_empty() {
            return;
        }
        if let Some(last) = self.history.back()
            && last == &input
        {
            return;
        }
        if self.history.len() >= INPUT_HISTORY_CAP {
            self.history.pop_front();
        }
        self.history.push_back(input);
    }

    /// Clear the input buffer
    pub fn clear(&mut self) {
        self.content.clear();
        self.cursor_position = 0;
    }

    /// Check if input is empty
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    /// Get the input content
    pub fn get(&self) -> &str {
        &self.content
    }

    /// Set the input content
    pub fn set(&mut self, content: impl Into<String>) {
        self.content = content.into();
        self.cursor_position = self.content.len();
    }

    /// Insert a character at cursor position
    pub fn insert(&mut self, c: char) {
        self.content.insert(self.cursor_position, c);
        self.cursor_position += c.len_utf8();
    }

    /// Insert a string at cursor position
    pub fn insert_str(&mut self, s: &str) {
        self.content.insert_str(self.cursor_position, s);
        self.cursor_position += s.len();
    }

    /// Delete character before cursor (backspace)
    pub fn backspace(&mut self) -> bool {
        if self.cursor_position > 0 {
            // Find the start of the previous character
            let prev_boundary = self.content[..self.cursor_position]
                .char_indices()
                .next_back()
                .map(|(idx, _)| idx)
                .unwrap_or(0);
            self.content.remove(prev_boundary);
            self.cursor_position = prev_boundary;
            true
        } else {
            false
        }
    }

    /// Delete character at cursor (delete key)
    pub fn delete(&mut self) -> bool {
        if self.cursor_position < self.content.len() {
            self.content.remove(self.cursor_position);
            true
        } else {
            false
        }
    }

    /// Move cursor left by one character
    pub fn move_left(&mut self) {
        if self.cursor_position > 0 {
            // Find the previous char boundary
            self.cursor_position = self.content[..self.cursor_position]
                .char_indices()
                .next_back()
                .map(|(idx, _)| idx)
                .unwrap_or(0);
        }
    }

    /// Move cursor right by one character
    pub fn move_right(&mut self) {
        if self.cursor_position < self.content.len() {
            // Find the next char boundary
            self.cursor_position = self.content[self.cursor_position..]
                .char_indices()
                .nth(1)
                .map(|(idx, _)| self.cursor_position + idx)
                .unwrap_or(self.content.len());
        }
    }

    /// Move cursor to start
    pub fn move_home(&mut self) {
        self.cursor_position = 0;
    }

    /// Move cursor to end
    pub fn move_end(&mut self) {
        self.cursor_position = self.content.len();
    }
}

impl Default for InputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_to_history_skips_empty_and_whitespace() {
        let mut buf = InputBuffer::new();
        buf.add_to_history(String::new());
        buf.add_to_history("   ".into());
        buf.add_to_history("\t\n".into());
        assert!(buf.history.is_empty());
    }

    #[test]
    fn add_to_history_dedups_consecutive() {
        let mut buf = InputBuffer::new();
        buf.add_to_history("hello".into());
        buf.add_to_history("hello".into());
        buf.add_to_history("world".into());
        buf.add_to_history("hello".into()); // non-consecutive duplicate is kept
        assert_eq!(
            buf.history.iter().cloned().collect::<Vec<_>>(),
            vec!["hello", "world", "hello"]
        );
    }

    #[test]
    fn add_to_history_caps_at_limit() {
        let mut buf = InputBuffer::new();
        for i in 0..(INPUT_HISTORY_CAP + 25) {
            buf.add_to_history(format!("msg{}", i));
        }
        assert_eq!(buf.history.len(), INPUT_HISTORY_CAP);
        // Oldest 25 evicted; first retained entry is msg25.
        assert_eq!(buf.history.front().unwrap(), "msg25");
        assert_eq!(
            buf.history.back().unwrap(),
            &format!("msg{}", INPUT_HISTORY_CAP + 24)
        );
    }
}
