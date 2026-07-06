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
    /// The first (0-based) line visible in the editor viewport. Tracked so a
    /// buffer taller than its pane can scroll to keep the cursor in view.
    scroll: usize,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            rope: Rope::new(),
            path: None,
            cursor: 0,
            dirty: false,
            scroll: 0,
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
            scroll: 0,
        }
    }

    /// The first visible line in the viewport (the vertical scroll offset).
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// Adjust the scroll offset by the minimum needed to keep the cursor line
    /// within a viewport of `height` rows, so navigating or editing past the
    /// bottom (or above the top) of the pane scrolls the buffer to follow.
    pub fn ensure_visible(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        let cursor_line = self.cursor_line_col().0;
        if cursor_line < self.scroll {
            // Cursor moved above the viewport — scroll up to it.
            self.scroll = cursor_line;
        } else if cursor_line >= self.scroll + height {
            // Cursor moved below the viewport — scroll down just enough.
            self.scroll = cursor_line + 1 - height;
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

    /// Move the cursor to the next occurrence of `needle` after the cursor,
    /// wrapping to the top of the buffer. Returns whether a match was found.
    /// The search is case-sensitive and does not modify the buffer.
    pub fn find_next(&mut self, needle: &str) -> bool {
        if needle.is_empty() {
            return false;
        }
        let text = self.rope.to_string();
        // Start one char past the cursor so repeated searches advance.
        let from_char = (self.cursor + 1).min(self.rope.len_chars());
        let from_byte = char_to_byte(&text, from_char);
        let hit = text[from_byte..]
            .find(needle)
            .map(|i| from_byte + i)
            .or_else(|| text.find(needle));
        match hit {
            Some(byte) => {
                self.cursor = text[..byte].chars().count();
                true
            }
            None => false,
        }
    }

    /// Move the cursor to the previous occurrence of `needle` before the cursor,
    /// wrapping to the bottom of the buffer. Returns whether a match was found.
    pub fn find_prev(&mut self, needle: &str) -> bool {
        if needle.is_empty() {
            return false;
        }
        let text = self.rope.to_string();
        let before_byte = char_to_byte(&text, self.cursor);
        let hit = text[..before_byte]
            .rfind(needle)
            .or_else(|| text.rfind(needle));
        match hit {
            Some(byte) => {
                self.cursor = text[..byte].chars().count();
                true
            }
            None => false,
        }
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

/// Byte offset of the `n`-th char in `text` (clamped to its end), for bridging
/// the char-indexed cursor to byte-indexed `str` search.
fn char_to_byte(text: &str, n: usize) -> usize {
    text.char_indices()
        .nth(n)
        .map(|(b, _)| b)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_next_and_prev_move_the_cursor_and_wrap() {
        let mut doc = Document::from_str(None, "foo bar foo baz foo");
        // Cursor starts at 0; find_next lands on the second `foo` (offset 8).
        assert!(doc.find_next("foo"));
        assert_eq!(doc.cursor(), 8);
        // Again advances to the third `foo` (offset 16).
        assert!(doc.find_next("foo"));
        assert_eq!(doc.cursor(), 16);
        // Past the last match it wraps to the first (offset 0).
        assert!(doc.find_next("foo"));
        assert_eq!(doc.cursor(), 0);

        // find_prev from the first match wraps to the last (offset 16).
        assert!(doc.find_prev("foo"));
        assert_eq!(doc.cursor(), 16);

        // A miss leaves the cursor put and reports false.
        let here = doc.cursor();
        assert!(!doc.find_next("qux"));
        assert_eq!(doc.cursor(), here);
        assert!(!doc.find_next(""));
    }

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
    fn ensure_visible_scrolls_to_follow_the_cursor() {
        // A 20-line buffer viewed through an 8-row pane.
        let text: String = (0..20).map(|n| format!("line {n}\n")).collect();
        let mut doc = Document::from_str(None, &text);
        assert_eq!(doc.scroll(), 0);

        // Cursor near the top stays put — no scroll needed.
        doc.ensure_visible(8);
        assert_eq!(doc.scroll(), 0);

        // Move the cursor to line 15 and re-check: the pane scrolls just enough
        // to bring line 15 onto the last visible row (15 + 1 - 8 = 8).
        for _ in 0..15 {
            doc.move_down();
        }
        doc.ensure_visible(8);
        assert_eq!(doc.scroll(), 8);

        // Moving back above the viewport scrolls up to the cursor.
        for _ in 0..12 {
            doc.move_up();
        }
        doc.ensure_visible(8);
        assert_eq!(doc.scroll(), 3);
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
