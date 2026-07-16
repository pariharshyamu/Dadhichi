//! A text document view-model backed by a rope.
//!
//! Editing is incremental: a [`ropey::Rope`] gives O(log n) inserts and deletes
//! regardless of file size, so a keystroke never rewrites the buffer. The cursor
//! is a single char index; line/column are derived on demand. An optional
//! *anchor* turns the cursor into a selection, and every mutation is journalled
//! onto an undo stack so edits are reversible. This is the model a GPU editor
//! would render; it holds no rendering state of its own.

use ropey::Rope;
use std::path::PathBuf;

/// How an edit is classified for undo coalescing. Consecutive edits of the same
/// kind that are physically contiguous fold into one undo step so a run of typed
/// characters (or a run of backspaces) is undone as a unit, the way every real
/// editor behaves. A [`Replace`](EditKind::Replace) never coalesces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditKind {
    /// Pure insertion (no text removed) — typing.
    Insert,
    /// Pure deletion (no text inserted) — backspace / forward-delete.
    Delete,
    /// A removal *and* an insertion in one shot — typing over a selection, paste
    /// over a selection. Discrete: it never merges with a neighbour.
    Replace,
}

/// A single reversible edit: at char position `at`, the text `removed` was
/// replaced by the text `inserted`. Undo restores `removed`; redo re-applies
/// `inserted`. The cursor/anchor snapshot lets undo put the caret back where it
/// was before the edit.
#[derive(Debug, Clone)]
struct Edit {
    at: usize,
    removed: String,
    inserted: String,
    cursor_before: usize,
    anchor_before: Option<usize>,
}

/// An open text buffer with a cursor, an optional selection, an undo history and
/// a dirty flag.
#[derive(Debug, Clone)]
pub struct Document {
    rope: Rope,
    /// The file this buffer is associated with, if any.
    pub path: Option<PathBuf>,
    /// Cursor position as a char offset into the rope.
    cursor: usize,
    /// The selection anchor, a char offset. `Some` while a selection is active:
    /// the selection spans `anchor..cursor` (in either order). `None` means no
    /// selection — just a caret.
    anchor: Option<usize>,
    /// Whether the buffer has unsaved changes.
    pub dirty: bool,
    /// The first (0-based) line visible in the editor viewport. Tracked so a
    /// buffer taller than its pane can scroll to keep the cursor in view.
    scroll: usize,
    /// Reversible edits, most recent last. `undo` pops from here.
    undo_stack: Vec<Edit>,
    /// Edits undone but not yet superseded, most recent last. `redo` pops from
    /// here; any fresh edit clears it.
    redo_stack: Vec<Edit>,
    /// The kind of the still-open undo group, if the last recorded edit may still
    /// coalesce with the next one. Movement, save, undo and redo close the group.
    open_group: Option<EditKind>,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            rope: Rope::new(),
            path: None,
            cursor: 0,
            anchor: None,
            dirty: false,
            scroll: 0,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            open_group: None,
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
            ..Self::default()
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

    /// The char offset at which line `n` begins, or the buffer end if `n` is past
    /// the last line. Lets a renderer map the char-indexed selection onto a line.
    pub fn line_start(&self, n: usize) -> usize {
        if n >= self.rope.len_lines() {
            return self.rope.len_chars();
        }
        self.rope.line_to_char(n)
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

    /// The selection anchor's char offset, if a selection is active.
    pub fn anchor(&self) -> Option<usize> {
        self.anchor
    }

    // ---- Selection --------------------------------------------------------

    /// The selected range as a normalised `(start, end)` char span, or `None`
    /// when there is no selection (no anchor, or the anchor coincides with the
    /// cursor so the range is empty).
    pub fn selection(&self) -> Option<(usize, usize)> {
        self.anchor
            .map(|a| {
                if a <= self.cursor {
                    (a, self.cursor)
                } else {
                    (self.cursor, a)
                }
            })
            .filter(|(s, e)| s != e)
    }

    /// Whether a non-empty selection is active.
    pub fn has_selection(&self) -> bool {
        self.selection().is_some()
    }

    /// The selected text, or `None` when there is no selection.
    pub fn selected_text(&self) -> Option<String> {
        self.selection().map(|(s, e)| self.rope.slice(s..e).to_string())
    }

    /// Drop the selection, keeping the cursor where it is.
    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }

    /// Select the whole buffer (anchor at the start, cursor at the end).
    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.rope.len_chars();
        self.open_group = None;
    }

    /// Delete the current selection, if any, as one undoable edit. Returns
    /// whether anything was removed. The cursor lands at the selection start.
    pub fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            return false;
        };
        self.edit(start, end - start, "", EditKind::Delete);
        self.cursor = start;
        self.anchor = None;
        // A discrete range deletion shouldn't fold into a later backspace run.
        self.open_group = None;
        true
    }

    // ---- Editing ----------------------------------------------------------

    /// Insert `text` at the cursor, advancing it. If a selection is active the
    /// selection is replaced by `text` in a single undoable step.
    pub fn insert(&mut self, text: &str) {
        if let Some((start, end)) = self.selection() {
            self.edit(start, end - start, text, EditKind::Replace);
            self.cursor = start + text.chars().count();
            self.anchor = None;
        } else {
            let at = self.cursor;
            self.edit(at, 0, text, EditKind::Insert);
            self.cursor = at + text.chars().count();
        }
    }

    /// Delete the character before the cursor (backspace), or the whole selection
    /// if one is active.
    pub fn backspace(&mut self) {
        if self.has_selection() {
            self.delete_selection();
            return;
        }
        if self.cursor > 0 {
            self.edit(self.cursor - 1, 1, "", EditKind::Delete);
            self.cursor -= 1;
        }
    }

    /// Delete the character at the cursor (forward delete), or the whole selection
    /// if one is active.
    pub fn delete(&mut self) {
        if self.has_selection() {
            self.delete_selection();
            return;
        }
        if self.cursor < self.rope.len_chars() {
            self.edit(self.cursor, 1, "", EditKind::Delete);
        }
    }

    /// Undo the most recent edit (or coalesced run of edits), restoring the text,
    /// the cursor and any selection to their pre-edit state. Returns whether
    /// there was anything to undo.
    pub fn undo(&mut self) -> bool {
        let Some(edit) = self.undo_stack.pop() else {
            return false;
        };
        // Invert: the edit replaced `removed` with `inserted`, so remove
        // `inserted` and put `removed` back.
        let ins_len = edit.inserted.chars().count();
        if ins_len > 0 {
            self.rope.remove(edit.at..edit.at + ins_len);
        }
        if !edit.removed.is_empty() {
            self.rope.insert(edit.at, &edit.removed);
        }
        let len = self.rope.len_chars();
        self.cursor = edit.cursor_before.min(len);
        self.anchor = edit.anchor_before.map(|a| a.min(len));
        self.redo_stack.push(edit);
        self.open_group = None;
        self.dirty = true;
        true
    }

    /// Re-apply the most recently undone edit. Returns whether there was anything
    /// to redo.
    pub fn redo(&mut self) -> bool {
        let Some(edit) = self.redo_stack.pop() else {
            return false;
        };
        let rem_len = edit.removed.chars().count();
        if rem_len > 0 {
            self.rope.remove(edit.at..edit.at + rem_len);
        }
        if !edit.inserted.is_empty() {
            self.rope.insert(edit.at, &edit.inserted);
        }
        let len = self.rope.len_chars();
        self.cursor = (edit.at + edit.inserted.chars().count()).min(len);
        self.anchor = None;
        self.undo_stack.push(edit);
        self.open_group = None;
        self.dirty = true;
        true
    }

    /// Close the current undo group so the next edit starts a fresh one. Callers
    /// use this after a discrete action (a paste, a programmatic edit) to keep it
    /// from coalescing with whatever the user types next.
    pub fn seal_undo_group(&mut self) {
        self.open_group = None;
    }

    /// Apply a text change to the rope and journal it for undo. Removes
    /// `remove_len` chars at `at`, then inserts `insert`. Records the removed text
    /// (captured *before* the removal) so the edit is reversible. Callers set the
    /// cursor/anchor afterwards; the pre-edit positions are snapshotted here.
    fn edit(&mut self, at: usize, remove_len: usize, insert: &str, kind: EditKind) {
        let removed = if remove_len > 0 {
            self.rope.slice(at..at + remove_len).to_string()
        } else {
            String::new()
        };
        let record = Edit {
            at,
            removed,
            inserted: insert.to_string(),
            cursor_before: self.cursor,
            anchor_before: self.anchor,
        };
        if remove_len > 0 {
            self.rope.remove(at..at + remove_len);
        }
        if !insert.is_empty() {
            self.rope.insert(at, insert);
        }
        self.record(record, kind);
        self.dirty = true;
    }

    /// Push an edit onto the undo stack, coalescing it into the open group when it
    /// continues a contiguous typing or deletion run. Clears the redo stack, since
    /// a new edit forks history.
    fn record(&mut self, edit: Edit, kind: EditKind) {
        self.redo_stack.clear();

        if self.open_group == Some(kind) {
            if let Some(prev) = self.undo_stack.last_mut() {
                match kind {
                    EditKind::Insert => {
                        // Extend a run of typing when the new text is inserted
                        // exactly where the previous run ended, and neither side
                        // crosses a newline (Enter starts a fresh undo unit).
                        let prev_end = prev.at + prev.inserted.chars().count();
                        let crosses_newline =
                            edit.inserted.contains('\n') || prev.inserted.ends_with('\n');
                        if prev_end == edit.at && !crosses_newline {
                            prev.inserted.push_str(&edit.inserted);
                            return;
                        }
                    }
                    EditKind::Delete => {
                        // Backspace run: each deletion sits immediately before the
                        // previous one — prepend and extend the group leftward.
                        if edit.at + edit.removed.chars().count() == prev.at {
                            prev.removed = format!("{}{}", edit.removed, prev.removed);
                            prev.at = edit.at;
                            return;
                        }
                        // Forward-delete run: the cursor stays put, so successive
                        // deletions share the same position — append.
                        if edit.at == prev.at {
                            prev.removed.push_str(&edit.removed);
                            return;
                        }
                    }
                    EditKind::Replace => {}
                }
            }
        }

        self.undo_stack.push(edit);
        // A replace is discrete; typing and deletion runs stay open to coalesce.
        self.open_group = match kind {
            EditKind::Replace => None,
            other => Some(other),
        };
    }

    // ---- Navigation -------------------------------------------------------

    /// Move the cursor to `target`, either extending the selection (`extend`) or
    /// collapsing it. Closes the open undo group so typing after a move is its own
    /// unit.
    fn go(&mut self, target: usize, extend: bool) {
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.cursor = target.min(self.rope.len_chars());
        self.open_group = None;
    }

    /// The cursor's target one char to the left.
    fn left_target(&self) -> usize {
        self.cursor.saturating_sub(1)
    }

    /// The cursor's target one char to the right.
    fn right_target(&self) -> usize {
        if self.cursor < self.rope.len_chars() {
            self.cursor + 1
        } else {
            self.cursor
        }
    }

    /// The cursor's target one line up, preserving column where possible, or the
    /// current position when already on the first line.
    fn up_target(&self) -> usize {
        let (line, col) = self.cursor_line_col();
        if line == 0 {
            self.cursor
        } else {
            self.clamp_to_line(line - 1, col)
        }
    }

    /// The cursor's target one line down, or the current position when already on
    /// the last line.
    fn down_target(&self) -> usize {
        let (line, col) = self.cursor_line_col();
        if line + 1 >= self.rope.len_lines() {
            self.cursor
        } else {
            self.clamp_to_line(line + 1, col)
        }
    }

    /// Move the cursor one char left, collapsing any selection.
    pub fn move_left(&mut self) {
        let t = self.left_target();
        self.go(t, false);
    }

    /// Move the cursor one char right, collapsing any selection.
    pub fn move_right(&mut self) {
        let t = self.right_target();
        self.go(t, false);
    }

    /// Move the cursor up one line, preserving column, collapsing any selection.
    pub fn move_up(&mut self) {
        let t = self.up_target();
        self.go(t, false);
    }

    /// Move the cursor down one line, preserving column, collapsing any selection.
    pub fn move_down(&mut self) {
        let t = self.down_target();
        self.go(t, false);
    }

    /// Move the cursor to the start of its line, collapsing any selection.
    pub fn move_line_start(&mut self) {
        let (line, _) = self.cursor_line_col();
        let t = self.rope.line_to_char(line);
        self.go(t, false);
    }

    /// Move the cursor to the end of its line (before the newline), collapsing any
    /// selection.
    pub fn move_line_end(&mut self) {
        let t = self.line_end_target();
        self.go(t, false);
    }

    /// Extend the selection one char left.
    pub fn select_left(&mut self) {
        let t = self.left_target();
        self.go(t, true);
    }

    /// Extend the selection one char right.
    pub fn select_right(&mut self) {
        let t = self.right_target();
        self.go(t, true);
    }

    /// Extend the selection one line up.
    pub fn select_up(&mut self) {
        let t = self.up_target();
        self.go(t, true);
    }

    /// Extend the selection one line down.
    pub fn select_down(&mut self) {
        let t = self.down_target();
        self.go(t, true);
    }

    /// Extend the selection to the start of the line.
    pub fn select_line_start(&mut self) {
        let (line, _) = self.cursor_line_col();
        let t = self.rope.line_to_char(line);
        self.go(t, true);
    }

    /// Extend the selection to the end of the line.
    pub fn select_line_end(&mut self) {
        let t = self.line_end_target();
        self.go(t, true);
    }

    /// The char offset of the end of the cursor's line, before any newline.
    fn line_end_target(&self) -> usize {
        let (line, _) = self.cursor_line_col();
        let start = self.rope.line_to_char(line);
        let len = self.line(line).map(|l| l.chars().count()).unwrap_or(0);
        start + len
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
                self.go(text[..byte].chars().count(), false);
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
                self.go(text[..byte].chars().count(), false);
                true
            }
            None => false,
        }
    }

    /// Mark the buffer clean (e.g. after a save). Also closes the undo group so
    /// edits made after a save don't coalesce with edits made before it.
    pub fn mark_saved(&mut self) {
        self.dirty = false;
        self.open_group = None;
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

    #[test]
    fn undo_and_redo_reverse_a_typing_run_as_one_unit() {
        let mut doc = Document::new();
        // Typing coalesces into a single undo group.
        for c in "hello".chars() {
            doc.insert(&c.to_string());
        }
        assert_eq!(doc.text(), "hello");

        // One undo removes the whole coalesced run and restores the caret.
        assert!(doc.undo());
        assert_eq!(doc.text(), "");
        assert_eq!(doc.cursor(), 0);

        // Redo re-applies it, cursor after the inserted text.
        assert!(doc.redo());
        assert_eq!(doc.text(), "hello");
        assert_eq!(doc.cursor(), 5);

        // Nothing left to redo.
        assert!(!doc.redo());
    }

    #[test]
    fn moving_the_cursor_breaks_the_undo_group() {
        let mut doc = Document::new();
        doc.insert("abc");
        doc.move_left(); // seals the "abc" group
        doc.insert("X"); // a fresh group at the new position
        assert_eq!(doc.text(), "abXc");

        // First undo peels only the second group.
        assert!(doc.undo());
        assert_eq!(doc.text(), "abc");
        // Second undo peels the first.
        assert!(doc.undo());
        assert_eq!(doc.text(), "");
        assert!(!doc.undo());
    }

    #[test]
    fn a_fresh_edit_clears_the_redo_stack() {
        let mut doc = Document::new();
        doc.insert("a");
        doc.undo();
        assert_eq!(doc.text(), "");
        // Editing after an undo forks history: the old redo is gone.
        doc.insert("b");
        assert!(!doc.redo());
        assert_eq!(doc.text(), "b");
    }

    #[test]
    fn backspace_run_undoes_together() {
        let mut doc = Document::from_str(None, "abcd");
        // Put the cursor at the end.
        doc.move_line_end();
        assert_eq!(doc.cursor(), 4);
        doc.backspace(); // d
        doc.backspace(); // c
        assert_eq!(doc.text(), "ab");
        // A single undo brings back both deleted chars.
        assert!(doc.undo());
        assert_eq!(doc.text(), "abcd");
        assert_eq!(doc.cursor(), 4);
    }

    #[test]
    fn selection_extends_with_shift_moves_and_reports_text() {
        let mut doc = Document::from_str(None, "hello world");
        assert!(!doc.has_selection());
        doc.select_right();
        doc.select_right();
        doc.select_right();
        assert_eq!(doc.selection(), Some((0, 3)));
        assert_eq!(doc.selected_text().as_deref(), Some("hel"));

        // A plain move collapses the selection.
        doc.move_right();
        assert!(!doc.has_selection());
    }

    #[test]
    fn typing_over_a_selection_replaces_it_and_undoes_in_one_step() {
        let mut doc = Document::from_str(None, "hello world");
        doc.select_right(); // select "h"
        doc.select_right(); // "he"
        doc.select_right(); // "hel"
        doc.select_right(); // "hell"
        doc.select_right(); // "hello"
        assert_eq!(doc.selected_text().as_deref(), Some("hello"));

        doc.insert("HI");
        assert_eq!(doc.text(), "HI world");
        assert!(!doc.has_selection());
        assert_eq!(doc.cursor(), 2);

        // Undo restores the replaced text and the selection.
        assert!(doc.undo());
        assert_eq!(doc.text(), "hello world");
        assert_eq!(doc.selection(), Some((0, 5)));
    }

    #[test]
    fn delete_selection_removes_the_range() {
        let mut doc = Document::from_str(None, "abcdef");
        doc.move_right();
        doc.select_right();
        doc.select_right(); // select "bc"
        assert_eq!(doc.selected_text().as_deref(), Some("bc"));
        assert!(doc.delete_selection());
        assert_eq!(doc.text(), "adef");
        assert_eq!(doc.cursor(), 1);
        assert!(!doc.has_selection());
        // With no selection, delete_selection is a no-op.
        assert!(!doc.delete_selection());
    }

    #[test]
    fn select_all_spans_the_buffer() {
        let mut doc = Document::from_str(None, "one\ntwo");
        doc.select_all();
        assert_eq!(doc.selection(), Some((0, 7)));
        assert_eq!(doc.selected_text().as_deref(), Some("one\ntwo"));
    }

    #[test]
    fn home_and_end_move_within_the_line() {
        let mut doc = Document::from_str(None, "abc\ndefgh");
        doc.move_down(); // line 1, col 0
        doc.move_line_end();
        assert_eq!(doc.cursor_line_col(), (1, 5));
        doc.move_line_start();
        assert_eq!(doc.cursor_line_col(), (1, 0));
    }
}
