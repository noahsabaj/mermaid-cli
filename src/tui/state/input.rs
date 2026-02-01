/// Input state management
///
/// User input buffer and cursor handling.

/// Input state - user input buffer and cursor
pub struct InputBuffer {
    /// User input buffer
    pub content: String,
    /// Cursor position in the input string
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
        self.cursor_position += 1;
    }

    /// Insert a string at cursor position
    pub fn insert_str(&mut self, s: &str) {
        self.content.insert_str(self.cursor_position, s);
        self.cursor_position += s.len();
    }

    /// Delete character before cursor (backspace)
    pub fn backspace(&mut self) -> bool {
        if self.cursor_position > 0 {
            self.cursor_position -= 1;
            self.content.remove(self.cursor_position);
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

    /// Move cursor left
    pub fn move_left(&mut self) {
        if self.cursor_position > 0 {
            self.cursor_position -= 1;
        }
    }

    /// Move cursor right
    pub fn move_right(&mut self) {
        if self.cursor_position < self.content.len() {
            self.cursor_position += 1;
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
