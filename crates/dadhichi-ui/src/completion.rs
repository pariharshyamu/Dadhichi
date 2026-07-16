//! The completion popup's view-model.
//!
//! The menu holds the full suggestion set the language server returned and a
//! filtered view of it driven by the word prefix under the cursor. As the user
//! keeps typing the prefix grows and the view narrows (no re-request needed);
//! deleting shrinks it back. Prefix matches rank before substring matches,
//! both case-insensitive. The menu closes itself when nothing matches.

/// One suggestion row: what's shown and what's inserted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompletionEntry {
    /// The identifier shown (and matched against the prefix).
    pub label: String,
    /// A short kind tag: `fn`, `var`, `struct`, …
    pub kind: String,
    /// Extra detail (a type signature, the defining module); may be empty.
    pub detail: String,
    /// The text inserted on accept.
    pub insert: String,
}

/// The completion popup: full item set, filtered view, and selection.
#[derive(Debug, Clone, Default)]
pub struct CompletionMenu {
    items: Vec<CompletionEntry>,
    /// Indices into `items` currently visible, in rank order.
    visible: Vec<usize>,
    /// Index into `visible` of the highlighted row.
    selected: usize,
    active: bool,
}

impl CompletionMenu {
    /// Open the menu with `items`, filtered by `prefix`. Does nothing (stays
    /// closed) when no item matches.
    pub fn open(&mut self, items: Vec<CompletionEntry>, prefix: &str) {
        self.items = items;
        self.active = true;
        self.refilter(prefix);
    }

    /// Whether the popup is showing.
    pub fn is_open(&self) -> bool {
        self.active
    }

    /// Close the popup, dropping its items.
    pub fn close(&mut self) {
        self.active = false;
        self.items.clear();
        self.visible.clear();
        self.selected = 0;
    }

    /// Re-rank the view for a new `prefix`: prefix matches first, then
    /// substring matches, both case-insensitive. An empty prefix shows all.
    /// Auto-closes when nothing matches (the popup never sits empty).
    pub fn refilter(&mut self, prefix: &str) {
        if !self.active {
            return;
        }
        let needle = prefix.to_lowercase();
        let mut starts: Vec<usize> = Vec::new();
        let mut contains: Vec<usize> = Vec::new();
        for (i, item) in self.items.iter().enumerate() {
            let hay = item.label.to_lowercase();
            if needle.is_empty() || hay.starts_with(&needle) {
                starts.push(i);
            } else if hay.contains(&needle) {
                contains.push(i);
            }
        }
        starts.extend(contains);
        self.visible = starts;
        self.selected = 0;
        if self.visible.is_empty() {
            self.close();
        }
    }

    /// The rows currently shown, in rank order.
    pub fn entries(&self) -> Vec<&CompletionEntry> {
        self.visible.iter().map(|&i| &self.items[i]).collect()
    }

    /// The index of the highlighted row (into [`entries`](Self::entries)).
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Highlight the next row, wrapping.
    pub fn select_next(&mut self) {
        if !self.visible.is_empty() {
            self.selected = (self.selected + 1) % self.visible.len();
        }
    }

    /// Highlight the previous row, wrapping.
    pub fn select_prev(&mut self) {
        if !self.visible.is_empty() {
            self.selected = (self.selected + self.visible.len() - 1) % self.visible.len();
        }
    }

    /// The highlighted entry, if the menu is open and non-empty.
    pub fn selected_entry(&self) -> Option<&CompletionEntry> {
        self.visible
            .get(self.selected)
            .map(|&i| &self.items[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(label: &str) -> CompletionEntry {
        CompletionEntry {
            label: label.into(),
            kind: "fn".into(),
            detail: String::new(),
            insert: format!("{label}()"),
        }
    }

    #[test]
    fn filters_rank_prefix_before_substring_and_narrow_as_typed() {
        let mut menu = CompletionMenu::default();
        menu.open(
            vec![entry("map"), entry("filter_map"), entry("flat_map"), entry("fold")],
            "ma",
        );
        // "map" starts with "ma"; the others merely contain it.
        let labels: Vec<&str> = menu.entries().iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, ["map", "filter_map", "flat_map"]);

        // A longer prefix narrows further; case-insensitive.
        menu.refilter("MAP");
        let labels: Vec<&str> = menu.entries().iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, ["map", "filter_map", "flat_map"]);

        // No match at all → the menu closes itself.
        menu.refilter("zzz");
        assert!(!menu.is_open());
    }

    #[test]
    fn selection_wraps_both_ways() {
        let mut menu = CompletionMenu::default();
        menu.open(vec![entry("a"), entry("b"), entry("c")], "");
        assert_eq!(menu.selected_entry().unwrap().label, "a");
        menu.select_prev();
        assert_eq!(menu.selected_entry().unwrap().label, "c", "wraps to end");
        menu.select_next();
        assert_eq!(menu.selected_entry().unwrap().label, "a", "wraps to start");
    }

    #[test]
    fn open_with_no_matches_stays_closed() {
        let mut menu = CompletionMenu::default();
        menu.open(vec![entry("alpha")], "zzz");
        assert!(!menu.is_open());
        assert!(menu.selected_entry().is_none());
    }
}
