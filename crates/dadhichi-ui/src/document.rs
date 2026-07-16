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

    // ---- Word-wise motion --------------------------------------------------

    /// The target one word to the left: skip whitespace, then a run of chars of
    /// the same class (identifier vs punctuation), the way VS Code's
    /// Ctrl+Left behaves. Newlines count as whitespace, so the motion crosses
    /// line boundaries.
    fn word_left_target(&self) -> usize {
        let mut i = self.cursor;
        while i > 0 && self.rope.char(i - 1).is_whitespace() {
            i -= 1;
        }
        if i > 0 {
            let class = is_word_char(self.rope.char(i - 1));
            while i > 0 {
                let c = self.rope.char(i - 1);
                if c.is_whitespace() || is_word_char(c) != class {
                    break;
                }
                i -= 1;
            }
        }
        i
    }

    /// The target one word to the right (mirror of [`word_left_target`]).
    fn word_right_target(&self) -> usize {
        let len = self.rope.len_chars();
        let mut i = self.cursor;
        while i < len && self.rope.char(i).is_whitespace() {
            i += 1;
        }
        if i < len {
            let class = is_word_char(self.rope.char(i));
            while i < len {
                let c = self.rope.char(i);
                if c.is_whitespace() || is_word_char(c) != class {
                    break;
                }
                i += 1;
            }
        }
        i
    }

    /// Move the cursor one word left, collapsing any selection.
    pub fn move_word_left(&mut self) {
        let t = self.word_left_target();
        self.go(t, false);
    }

    /// Move the cursor one word right, collapsing any selection.
    pub fn move_word_right(&mut self) {
        let t = self.word_right_target();
        self.go(t, false);
    }

    /// Extend the selection one word left.
    pub fn select_word_left(&mut self) {
        let t = self.word_left_target();
        self.go(t, true);
    }

    /// Extend the selection one word right.
    pub fn select_word_right(&mut self) {
        let t = self.word_right_target();
        self.go(t, true);
    }

    /// Delete from the previous word boundary to the cursor (Ctrl+Backspace) as
    /// one discrete undo step. With a selection active, deletes the selection.
    pub fn delete_word_back(&mut self) {
        if self.has_selection() {
            self.delete_selection();
            return;
        }
        let target = self.word_left_target();
        if target < self.cursor {
            self.edit(target, self.cursor - target, "", EditKind::Delete);
            self.cursor = target;
            self.open_group = None;
        }
    }

    // ---- Whole-line operations ----------------------------------------------

    /// The char span `[start, end)` of line `l`, where `end` is the start of the
    /// next line (so the span includes the trailing newline when there is one).
    fn line_span(&self, l: usize) -> (usize, usize) {
        let start = self.rope.line_to_char(l);
        let end = if l + 1 < self.rope.len_lines() {
            self.rope.line_to_char(l + 1)
        } else {
            self.rope.len_chars()
        };
        (start, end)
    }

    /// Duplicate the cursor's line below it, moving the cursor into the copy at
    /// the same column, as one undo step.
    pub fn duplicate_line(&mut self) {
        let (line, col) = self.cursor_line_col();
        let (start, end) = self.line_span(line);
        if start == end {
            return; // the phantom empty line after a trailing newline
        }
        let text = self.rope.slice(start..end).to_string();
        if text.ends_with('\n') {
            self.edit(end, 0, &text, EditKind::Replace);
            self.cursor = end + col;
        } else {
            // Last line without a newline: the copy needs a separator.
            let insert = format!("\n{text}");
            self.edit(end, 0, &insert, EditKind::Replace);
            self.cursor = end + 1 + col;
        }
        self.anchor = None;
        self.open_group = None;
    }

    /// Swap the cursor's line with the one above, keeping the cursor on its line
    /// (VS Code's Alt+Up). One undo step; no-op on the first line.
    pub fn move_line_up(&mut self) {
        let (line, col) = self.cursor_line_col();
        if line == 0 {
            return;
        }
        self.swap_lines(line - 1, line);
        self.cursor = self.rope.line_to_char(line - 1) + col;
        self.anchor = None;
        self.open_group = None;
    }

    /// Swap the cursor's line with the one below (VS Code's Alt+Down). One undo
    /// step; no-op on the last line.
    pub fn move_line_down(&mut self) {
        let (line, col) = self.cursor_line_col();
        if line + 1 >= self.rope.len_lines() {
            return;
        }
        let (next_start, next_end) = self.line_span(line + 1);
        if next_start == next_end {
            return; // below is only the phantom line after a trailing newline
        }
        self.swap_lines(line, line + 1);
        self.cursor = self.rope.line_to_char(line + 1) + col;
        self.anchor = None;
        self.open_group = None;
    }

    /// Replace lines `a` and `a+1 == b` with each other, normalising the trailing
    /// newline so swapping with a final newline-less line stays line-shaped.
    fn swap_lines(&mut self, a: usize, b: usize) {
        debug_assert_eq!(a + 1, b);
        let (a_start, a_end) = self.line_span(a);
        let (_, b_end) = self.line_span(b);
        let mut first = self.rope.slice(a_start..a_end).to_string();
        let mut second = self.rope.slice(a_end..b_end).to_string();
        if !second.ends_with('\n') {
            // The lower line lacked a newline; after the swap it sits on top and
            // needs one, while the upper line (now last) sheds its own.
            second.push('\n');
            first.pop();
        }
        let swapped = format!("{second}{first}");
        self.edit(a_start, b_end - a_start, &swapped, EditKind::Replace);
    }

    /// Delete the cursor's whole line (VS Code's Ctrl+Shift+K) as one undo step,
    /// keeping the cursor at the same column on the line that takes its place.
    pub fn delete_line(&mut self) {
        let (line, col) = self.cursor_line_col();
        let (mut start, end) = self.line_span(line);
        if start == end && start > 0 {
            // The phantom empty line after a trailing newline: deleting it means
            // removing that newline.
            start -= 1;
        }
        if start == end {
            return; // empty buffer
        }
        let remove = if end == self.rope.len_chars() && start > 0 && line > 0 {
            // Deleting the last line also removes the newline that preceded it,
            // so the buffer doesn't keep a dangling blank line.
            start -= 1;
            end - start
        } else {
            end - start
        };
        self.edit(start, remove, "", EditKind::Replace);
        let last = self.rope.len_lines().saturating_sub(1);
        self.cursor = self.clamp_to_line(line.min(last), col);
        self.anchor = None;
        self.open_group = None;
    }

    /// Toggle a line comment (`prefix`, e.g. `//` or `#`) on the selected lines,
    /// or the cursor's line when nothing is selected, as one undo step. If every
    /// non-blank line in the range is already commented the prefixes are removed;
    /// otherwise each non-blank line gains `prefix ` after its indentation.
    pub fn toggle_comment(&mut self, prefix: &str) {
        let (line, col) = self.cursor_line_col();
        let (first, last) = match self.selection() {
            Some((s, e)) => (
                self.rope.char_to_line(s),
                // `e` is exclusive; a selection ending at a line's col 0 does not
                // include that line.
                self.rope.char_to_line(e.saturating_sub(1).max(s)),
            ),
            None => (line, line),
        };
        let start = self.rope.line_to_char(first);
        let (_, end) = self.line_span(last);
        let span = self.rope.slice(start..end).to_string();

        let lines: Vec<&str> = span.split_inclusive('\n').collect();
        let mut non_blank = lines.iter().filter(|l| !l.trim().is_empty()).peekable();
        let all_commented = non_blank.peek().is_some()
            && non_blank.all(|l| l.trim_start().starts_with(prefix));

        let rebuilt: String = lines
            .iter()
            .map(|l| {
                if l.trim().is_empty() {
                    (*l).to_string()
                } else {
                    let ws = l.len() - l.trim_start().len();
                    let (indent, rest) = l.split_at(ws);
                    if all_commented {
                        let rest = rest.strip_prefix(prefix).unwrap_or(rest);
                        let rest = rest.strip_prefix(' ').unwrap_or(rest);
                        format!("{indent}{rest}")
                    } else {
                        format!("{indent}{prefix} {rest}")
                    }
                }
            })
            .collect();

        if rebuilt != span {
            self.edit(start, end - start, &rebuilt, EditKind::Replace);
            self.cursor = self.clamp_to_line(line, col);
            self.anchor = None;
            self.open_group = None;
        }
    }

    /// Insert a newline carrying the current line's leading whitespace, plus one
    /// indent level when the cursor sits right after an opening `{`/`(`/`[` or a
    /// `:` — the auto-indent every code editor performs on Enter.
    pub fn insert_newline(&mut self) {
        let (line, col) = self.cursor_line_col();
        let text = self.line(line).unwrap_or_default();
        // Only the whitespace left of the cursor: pressing Enter inside the
        // indentation shouldn't copy indentation the cursor hasn't passed.
        let indent: String = text
            .chars()
            .take(col)
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let opens_block = text
            .chars()
            .take(col)
            .last()
            .is_some_and(|c| matches!(c, '{' | '(' | '[' | ':'));
        let extra = if opens_block { "    " } else { "" };
        self.insert(&format!("\n{indent}{extra}"));
        self.open_group = None;
    }

    /// Indent: with a selection, prepend four spaces to every selected line as
    /// one undo step; otherwise insert four spaces at the cursor.
    pub fn indent(&mut self) {
        let Some((s, e)) = self.selection() else {
            self.insert("    ");
            return;
        };
        let first = self.rope.char_to_line(s);
        let last = self.rope.char_to_line(e.saturating_sub(1).max(s));
        let start = self.rope.line_to_char(first);
        let (_, end) = self.line_span(last);
        let span = self.rope.slice(start..end).to_string();
        let rebuilt: String = span
            .split_inclusive('\n')
            .map(|l| {
                if l.trim().is_empty() {
                    l.to_string()
                } else {
                    format!("    {l}")
                }
            })
            .collect();
        let (line, col) = self.cursor_line_col();
        self.edit(start, end - start, &rebuilt, EditKind::Replace);
        self.cursor = self.clamp_to_line(line, col + 4);
        self.anchor = None;
        self.open_group = None;
    }

    // ---- Paging & jumps ------------------------------------------------------

    /// Move the cursor `rows` lines up or down (PageUp/PageDown), preserving the
    /// column, optionally extending the selection.
    pub fn move_page(&mut self, down: bool, rows: usize, extend: bool) {
        let (line, col) = self.cursor_line_col();
        let target_line = if down {
            (line + rows).min(self.rope.len_lines().saturating_sub(1))
        } else {
            line.saturating_sub(rows)
        };
        let t = self.clamp_to_line(target_line, col);
        self.go(t, extend);
    }

    /// Move the cursor to the start of the buffer (Ctrl+Home), optionally
    /// extending the selection.
    pub fn move_doc_start(&mut self, extend: bool) {
        self.go(0, extend);
    }

    /// Move the cursor to the end of the buffer (Ctrl+End), optionally extending
    /// the selection.
    pub fn move_doc_end(&mut self, extend: bool) {
        let t = self.rope.len_chars();
        self.go(t, extend);
    }

    /// Jump the cursor to the start of 1-based line `n`, clamped to the buffer.
    pub fn goto_line(&mut self, n: usize) {
        let line = n.saturating_sub(1).min(self.rope.len_lines().saturating_sub(1));
        let t = self.rope.line_to_char(line);
        self.go(t, false);
    }

    /// How the cursor relates to the matches of `needle`: `(current, total)`,
    /// where `current` is the 1-based index of the match at or before the cursor
    /// (0 when the cursor precedes every match). Drives the find bar's `k/n`.
    pub fn find_stats(&self, needle: &str) -> (usize, usize) {
        if needle.is_empty() {
            return (0, 0);
        }
        let text = self.rope.to_string();
        let cursor_byte = char_to_byte(&text, self.cursor);
        let mut total = 0;
        let mut current = 0;
        for (byte, _) in text.match_indices(needle) {
            total += 1;
            if byte <= cursor_byte {
                current = total;
            }
        }
        (current, total)
    }

    /// Resolve `(line, col)` to a char offset, clamping the column to the line.
    fn clamp_to_line(&self, line: usize, col: usize) -> usize {
        let line_start = self.rope.line_to_char(line);
        let line_len = self.line(line).map(|l| l.chars().count()).unwrap_or(0);
        line_start + col.min(line_len)
    }
}

/// Whether `c` belongs to a word (identifier) rather than punctuation, for
/// word-wise motion. Whitespace is neither and is handled separately.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
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

    #[test]
    fn word_motion_hops_identifiers_and_punctuation() {
        let mut doc = Document::from_str(None, "let foo_bar = baz();");
        doc.move_word_right(); // past "let" → 3
        assert_eq!(doc.cursor(), 3);
        doc.move_word_right(); // past "foo_bar" → 11
        assert_eq!(doc.cursor(), 11);
        doc.move_word_right(); // past "=" → 13
        assert_eq!(doc.cursor(), 13);
        doc.move_word_right(); // past "baz" → 17
        assert_eq!(doc.cursor(), 17);

        doc.move_word_left(); // back to the start of "baz"
        assert_eq!(doc.cursor(), 14);

        // Shift+Ctrl+Left extends a selection over the word.
        doc.move_word_right();
        doc.select_word_left();
        assert_eq!(doc.selected_text().as_deref(), Some("baz"));
    }

    #[test]
    fn delete_word_back_removes_one_word_per_undo_step() {
        let mut doc = Document::from_str(None, "hello brave world");
        doc.move_doc_end(false);
        doc.delete_word_back();
        assert_eq!(doc.text(), "hello brave ");
        doc.delete_word_back();
        assert_eq!(doc.text(), "hello ");
        // Each Ctrl+Backspace is a discrete undo step.
        assert!(doc.undo());
        assert_eq!(doc.text(), "hello brave ");
        assert!(doc.undo());
        assert_eq!(doc.text(), "hello brave world");
    }

    #[test]
    fn duplicate_line_copies_below_and_moves_the_cursor_into_the_copy() {
        let mut doc = Document::from_str(None, "one\ntwo");
        doc.move_right(); // line 0, col 1
        doc.duplicate_line();
        assert_eq!(doc.text(), "one\none\ntwo");
        assert_eq!(doc.cursor_line_col(), (1, 1));

        // Duplicating the (newline-less) last line stays line-shaped.
        doc.move_down();
        doc.move_down(); // onto "two"
        doc.duplicate_line();
        assert_eq!(doc.text(), "one\none\ntwo\ntwo");

        // One undo per duplicate.
        assert!(doc.undo());
        assert_eq!(doc.text(), "one\none\ntwo");
    }

    #[test]
    fn move_line_up_and_down_swap_neighbours() {
        let mut doc = Document::from_str(None, "a\nb\nc");
        doc.move_down(); // on "b"
        doc.move_line_up();
        assert_eq!(doc.text(), "b\na\nc");
        assert_eq!(doc.cursor_line_col(), (0, 0), "cursor rides its line");

        doc.move_line_down();
        assert_eq!(doc.text(), "a\nb\nc");
        assert_eq!(doc.cursor_line_col(), (1, 0));

        // Moving the newline-less last line up keeps the buffer line-shaped.
        doc.move_down(); // on "c"
        doc.move_line_up();
        assert_eq!(doc.text(), "a\nc\nb");

        // Boundary no-ops: first line can't go up, last can't go down.
        let mut top = Document::from_str(None, "x\ny");
        top.move_line_up();
        assert_eq!(top.text(), "x\ny");
        top.move_down();
        top.move_line_down();
        assert_eq!(top.text(), "x\ny");
    }

    #[test]
    fn delete_line_removes_the_whole_line() {
        let mut doc = Document::from_str(None, "one\ntwo\nthree");
        doc.move_down(); // on "two"
        doc.delete_line();
        assert_eq!(doc.text(), "one\nthree");
        assert_eq!(doc.cursor_line_col(), (1, 0));

        // Deleting the last line also drops the preceding newline.
        doc.delete_line();
        assert_eq!(doc.text(), "one");

        // Undo restores each deletion.
        assert!(doc.undo());
        assert_eq!(doc.text(), "one\nthree");
    }

    #[test]
    fn toggle_comment_adds_and_removes_prefixes() {
        let mut doc = Document::from_str(None, "    let x = 1;");
        doc.toggle_comment("//");
        assert_eq!(doc.text(), "    // let x = 1;");
        doc.toggle_comment("//");
        assert_eq!(doc.text(), "    let x = 1;");

        // A multi-line selection comments every non-blank line; blank lines are
        // untouched. A mixed range (some commented) gets commented throughout.
        let mut doc = Document::from_str(None, "a\n\nb\n");
        doc.select_all();
        doc.toggle_comment("#");
        assert_eq!(doc.text(), "# a\n\n# b\n");
        doc.select_all();
        doc.toggle_comment("#");
        assert_eq!(doc.text(), "a\n\nb\n");
    }

    #[test]
    fn enter_auto_indents_and_opens_blocks() {
        let mut doc = Document::from_str(None, "    foo {");
        doc.move_line_end();
        doc.insert_newline();
        // Carries the 4-space indent and adds a level for the open brace.
        assert_eq!(doc.text(), "    foo {\n        ");
        assert_eq!(doc.cursor_line_col(), (1, 8));

        // Plain continuation keeps only the existing indent.
        doc.insert("bar");
        doc.insert_newline();
        assert_eq!(doc.line(2).as_deref(), Some("        "));
    }

    #[test]
    fn indent_inserts_spaces_or_indents_selected_lines() {
        let mut doc = Document::from_str(None, "a");
        doc.indent();
        assert_eq!(doc.text(), "    a");

        let mut doc = Document::from_str(None, "a\nb");
        doc.select_all();
        doc.indent();
        assert_eq!(doc.text(), "    a\n    b");
        // One undo step for the block indent.
        assert!(doc.undo());
        assert_eq!(doc.text(), "a\nb");
    }

    #[test]
    fn page_moves_and_document_jumps() {
        let text: String = (0..50).map(|n| format!("line {n}\n")).collect();
        let mut doc = Document::from_str(None, &text);
        doc.move_page(true, 20, false);
        assert_eq!(doc.cursor_line_col().0, 20);
        doc.move_page(false, 5, false);
        assert_eq!(doc.cursor_line_col().0, 15);

        doc.move_doc_end(false);
        assert_eq!(doc.cursor(), doc.text().chars().count());
        doc.move_doc_start(false);
        assert_eq!(doc.cursor(), 0);

        // goto_line is 1-based and clamps past the end.
        doc.goto_line(10);
        assert_eq!(doc.cursor_line_col().0, 9);
        doc.goto_line(9999);
        assert_eq!(doc.cursor_line_col().0, doc.line_count() - 1);
    }

    #[test]
    fn find_stats_report_current_of_total() {
        let mut doc = Document::from_str(None, "foo bar foo baz foo");
        assert_eq!(doc.find_stats("foo"), (1, 3), "cursor at 0 sits on match 1");
        doc.find_next("foo");
        assert_eq!(doc.find_stats("foo"), (2, 3));
        doc.find_next("foo");
        assert_eq!(doc.find_stats("foo"), (3, 3));
        assert_eq!(doc.find_stats("zzz"), (0, 0));
    }
}
