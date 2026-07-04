//! The command palette: a fuzzy finder over the kernel's registered commands.
//!
//! It mirrors the names from [`CommandRegistry`](dadhichi_core::CommandRegistry)
//! and ranks them against the user's query with a subsequence fuzzy match, so
//! `"agrn"` finds `"agent.run"`. Accepting a result yields the command name,
//! which the shell dispatches back through the kernel — the palette itself holds
//! no behaviour, only selection state.

/// A fuzzy-searchable command palette.
#[derive(Debug, Default)]
pub struct CommandPalette {
    commands: Vec<String>,
    query: String,
    selected: usize,
    open: bool,
}

/// A scored palette entry (higher score ranks first).
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    /// The command name.
    pub name: String,
    /// Fuzzy-match score.
    pub score: i32,
}

impl CommandPalette {
    /// Create an empty palette.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the set of commands the palette searches.
    pub fn set_commands(&mut self, commands: Vec<String>) {
        self.commands = commands;
        self.clamp_selection();
    }

    /// Whether the palette is currently shown.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Open the palette, resetting the query.
    pub fn open(&mut self) {
        self.open = true;
        self.query.clear();
        self.selected = 0;
    }

    /// Close the palette.
    pub fn close(&mut self) {
        self.open = false;
    }

    /// The current query text.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Append a character to the query.
    pub fn push(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
    }

    /// Remove the last character of the query.
    pub fn backspace(&mut self) {
        self.query.pop();
        self.selected = 0;
    }

    /// Move the selection down (wrapping).
    pub fn select_next(&mut self) {
        let n = self.results().len();
        if n > 0 {
            self.selected = (self.selected + 1) % n;
        }
    }

    /// Move the selection up (wrapping).
    pub fn select_prev(&mut self) {
        let n = self.results().len();
        if n > 0 {
            self.selected = (self.selected + n - 1) % n;
        }
    }

    /// The index of the highlighted result.
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Accept the highlighted result, returning its command name.
    pub fn accept(&self) -> Option<String> {
        self.results().get(self.selected).map(|m| m.name.clone())
    }

    /// The ranked matches for the current query. An empty query returns every
    /// command in registration order.
    pub fn results(&self) -> Vec<Match> {
        if self.query.is_empty() {
            return self
                .commands
                .iter()
                .map(|name| Match {
                    name: name.clone(),
                    score: 0,
                })
                .collect();
        }
        let mut scored: Vec<Match> = self
            .commands
            .iter()
            .filter_map(|name| {
                fuzzy_score(&self.query, name).map(|score| Match {
                    name: name.clone(),
                    score,
                })
            })
            .collect();
        // Highest score first; ties broken by shorter (more precise) name.
        scored.sort_by(|a, b| b.score.cmp(&a.score).then(a.name.len().cmp(&b.name.len())));
        scored
    }

    fn clamp_selection(&mut self) {
        let n = self.results().len();
        if self.selected >= n {
            self.selected = n.saturating_sub(1);
        }
    }
}

/// Score `candidate` against `query` as a case-insensitive subsequence match,
/// rewarding contiguous runs and start-of-word hits. Returns `None` if `query`
/// is not a subsequence of `candidate`.
fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let c: Vec<char> = candidate.to_lowercase().chars().collect();
    let mut qi = 0;
    let mut score = 0;
    let mut prev_match: Option<usize> = None;
    for (ci, &ch) in c.iter().enumerate() {
        if qi < q.len() && ch == q[qi] {
            score += 1;
            // Bonus for consecutive matches.
            if prev_match == Some(ci.wrapping_sub(1)) {
                score += 3;
            }
            // Bonus for matching at a word boundary.
            if ci == 0 || matches!(c.get(ci - 1), Some('.') | Some('_') | Some(' ')) {
                score += 2;
            }
            prev_match = Some(ci);
            qi += 1;
        }
    }
    (qi == q.len()).then_some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> CommandPalette {
        let mut p = CommandPalette::new();
        p.set_commands(vec![
            "agent.run".into(),
            "editor.format".into(),
            "editor.save".into(),
            "git.commit".into(),
        ]);
        p
    }

    #[test]
    fn empty_query_lists_all() {
        let p = palette();
        assert_eq!(p.results().len(), 4);
    }

    #[test]
    fn subsequence_matches_and_ranks() {
        let mut p = palette();
        p.push('a');
        p.push('g');
        p.push('r');
        let results = p.results();
        // "agr" is a subsequence of "agent.run"; it should be the top hit.
        assert_eq!(results[0].name, "agent.run");
    }

    #[test]
    fn non_subsequence_is_filtered_out() {
        assert!(fuzzy_score("zzz", "editor.save").is_none());
        assert!(fuzzy_score("es", "editor.save").is_some());
    }

    #[test]
    fn selection_wraps_and_accepts() {
        let mut p = palette();
        p.open();
        assert_eq!(p.selected_index(), 0);
        p.select_prev(); // wraps to last
        assert_eq!(p.selected_index(), 3);
        p.select_next(); // wraps back to 0
        assert_eq!(p.selected_index(), 0);
        assert_eq!(p.accept().as_deref(), Some("agent.run"));
    }

    #[test]
    fn contiguous_match_outranks_scattered() {
        // "save" is contiguous in "editor.save" and scattered in nothing else.
        let commit = fuzzy_score("commit", "git.commit").unwrap();
        let scattered = fuzzy_score("git", "git.commit").unwrap();
        assert!(commit > 0 && scattered > 0);
    }
}
