//! A text document view-model backed by a rope.
//!
//! Editing is incremental: a [`ropey::Rope`] gives O(log n) inserts and deletes
//! regardless of file size, so a keystroke never rewrites the buffer. The cursor
//! is a single char index; line/column are derived on demand. This is the model
//! a GPU editor would render; it holds no rendering state of its own.

use ropey::Rope;
use std::path::PathBuf;

/// An open text buffer with a cursor and dirty flag.
#[derive(Debug, Clone)]
pub struct Document {
    rope: Rope,
    /// The file this buffer is associated with, if any.
    pub path: Option<PathBuf>,
    /// Cursor position as a char offset into the rope.
    cursor: usize,
    /// Whether the buffer has unsaved changes.
    pub dirty: bool,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            rope: Rope::new(),
            path: None,
            cursor: 0,
            dirty: false,
        }
    }
}

impl Document {
    /// An empty scratch document.
    pub fn new() -> Self {
        Self::default()
    }

    /// A document seeded with `text`, associated with `path`.
    pub fn from_str(path: Option<PathBuf>, text: &str) -> Self {
        Self {
            rope: Rope::from_str(text),
            path,
            cursor: 0,
            dirty: false,
        }
    }

    /// The full buffer contents.
    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    /// Number of lines in the buffer.
    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    /// The `n`-th line (0-based) without its trailing newline, or `None`.
    pub fn line(&self, n: usize) -> Option<String> {
        if n >= self.rope.len_lines() {
            return None;
        }
        let line = self.rope.line(n).to_string();
        Some(line.trim_end_matches(['\n', '\r']).to_string())
    }

    /// The cursor's `(line, column)`, both 0-based.
    pub fn cursor_line_col(&self) -> (usize, usize) {
        let line = self.rope.char_to_line(self.cursor);
        let line_start = self.rope.line_to_char(line);
        (line, self.cursor - line_start)
    }

    /// The cursor's char offset.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Insert `text` at the cursor, advancing it.
    pub fn insert(&mut self, text: &str) {
        self.rope.insert(self.cursor, text);
        self.cursor += text.chars().count();
        self.dirty = true;
    }

    /// Delete the character before the cursor (backspace).
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.rope.remove(self.cursor - 1..self.cursor);
            self.cursor -= 1;
            self.dirty = true;
        }
    }

    /// Delete the character at the cursor (forward delete).
    pub fn delete(&mut self) {
        if self.cursor < self.rope.len_chars() {
            self.rope.remove(self.cursor..self.cursor + 1);
            self.dirty = true;
        }
    }

    /// Move the cursor one char left.
    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Move the cursor one char right.
    pub fn move_right(&mut self) {
        if self.cursor < self.rope.len_chars() {
            self.cursor += 1;
        }
    }

    /// Move the cursor up one line, preserving column where possible.
    pub fn move_up(&mut self) {
        let (line, col) = self.cursor_line_col();
        if line == 0 {
            return;
        }
        self.cursor = self.clamp_to_line(line - 1, col);
    }

    /// Move the cursor down one line, preserving column where possible.
    pub fn move_down(&mut self) {
        let (line, col) = self.cursor_line_col();
        if line + 1 >= self.rope.len_lines() {
            return;
        }
        self.cursor = self.clamp_to_line(line + 1, col);
    }

    /// Mark the buffer clean (e.g. after a save).
    pub fn mark_saved(&mut self) {
        self.dirty = false;
    }

    /// Resolve `(line, col)` to a char offset, clamping the column to the line.
    fn clamp_to_line(&self, line: usize, col: usize) -> usize {
        let line_start = self.rope.line_to_char(line);
        let line_len = self.line(line).map(|l| l.chars().count()).unwrap_or(0);
        line_start + col.min(line_len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_backspace_track_cursor() {
        let mut doc = Document::new();
        doc.insert("hello");
        assert_eq!(doc.text(), "hello");
        assert_eq!(doc.cursor(), 5);
        assert!(doc.dirty);

        doc.backspace();
        assert_eq!(doc.text(), "hell");
        assert_eq!(doc.cursor(), 4);
    }

    #[test]
    fn line_and_column_navigation() {
        let mut doc = Document::from_str(None, "abc\nde\nfghi");
        assert_eq!(doc.line_count(), 3);
        assert_eq!(doc.line(1).as_deref(), Some("de"));

        // Cursor starts at 0,0. Move down twice, keeping column.
        doc.move_right(); // col 1
        doc.move_right(); // col 2
        doc.move_down(); // line 1, but "de" only has len 2 → col clamped to 2
        assert_eq!(doc.cursor_line_col(), (1, 2));
        doc.move_down(); // line 2, col preserved at 2
        assert_eq!(doc.cursor_line_col(), (2, 2));
    }

    #[test]
    fn delete_forward_removes_at_cursor() {
        let mut doc = Document::from_str(None, "abc");
        doc.delete();
        assert_eq!(doc.text(), "bc");
    }
}
