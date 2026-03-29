//! Input state management
//!
//! User input buffer and cursor handling.

/// Input state - user input buffer and cursor
///
/// All cursor positions are byte offsets that are guaranteed to sit on
/// UTF-8 char boundaries. Methods navigate by whole characters, not
/// raw bytes, so multi-byte input (emoji, CJK, accented chars) is safe.
pub struct InputBuffer {
    /// User input buffer
    pub content: String,
    /// Cursor position as a **byte offset** (always on a char boundary)
    pub cursor_position: usize,
}

impl InputBuffer {
    /// Create a new empty input buffer
    pub fn new() -> Self {
        Self {
            content: String::new(),
            cursor_position: 0,
        }
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
