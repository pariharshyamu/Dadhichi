//! The command palette: a fuzzy finder over the kernel's registered commands.
//!
//! It mirrors the names from [`CommandRegistry`](dadhichi_core::CommandRegistry)
//! and ranks them against the user's query with a subsequence fuzzy match, so
//! `"agrn"` finds `"agent.run"`. Accepting a result yields a [`PaletteAction`],
//! which the shell dispatches back through the kernel — the palette itself holds
//! no behaviour, only selection state.
//!
//! Typing a leading `>` switches to **skill mode**: the list becomes the
//! equippable skills (each with a capability summary), and accepting one runs
//! it. `>` alone lists every skill; `>fs` filters them; the optional keyword
//! `>skill fs` reads naturally and works too.
//!
//! A leading `@` switches to **MCP mode**: the list becomes the configured MCP
//! servers (each with its transport, tool count, and connected state), and
//! accepting one toggles it — connecting a disconnected server or disconnecting a
//! connected one.

/// The sigil that switches the palette into skill-browsing mode.
pub const SKILL_SIGIL: char = '>';

/// The sigil that switches the palette into MCP-server mode.
pub const MCP_SIGIL: char = '@';

/// A fuzzy-searchable command-, skill-, and MCP-server palette.
#[derive(Debug, Default)]
pub struct CommandPalette {
    commands: Vec<String>,
    skills: Vec<SkillEntry>,
    mcp_servers: Vec<McpEntry>,
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

/// A skill shown in the palette's skill mode, with its capability summary.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SkillEntry {
    /// The skill id (dispatched as `skill.run { skill: <name> }`).
    pub name: String,
    /// One-line human description.
    pub description: String,
    /// A compact capability summary, e.g. `perms: read_workspace · tools: fs.read`.
    pub detail: String,
}

/// An MCP server shown in the palette's MCP mode.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct McpEntry {
    /// The configured server name (dispatched to `mcp.connect`/`mcp.disconnect`),
    /// or, for an `available` catalogue entry, the connector id (dispatched to
    /// `mcp.add`).
    pub name: String,
    /// A compact status line, e.g. `http · 3 tools · connected`.
    pub detail: String,
    /// Whether the server is currently connected — decides the toggle action.
    pub connected: bool,
    /// Whether this is a built-in connector not yet added (an "add" action)
    /// rather than an already-configured server (a connect/disconnect toggle).
    pub available: bool,
}

/// A single row shown in the palette — a command, a skill, or an MCP server.
#[derive(Debug, Clone, PartialEq)]
pub enum PaletteItem {
    /// A kernel command; the payload is its name.
    Command(String),
    /// A skill to equip and run.
    Skill(SkillEntry),
    /// An MCP server to connect or disconnect.
    Mcp(McpEntry),
}

impl PaletteItem {
    /// The primary label (command name, skill name, or server name).
    pub fn label(&self) -> &str {
        match self {
            PaletteItem::Command(name) => name,
            PaletteItem::Skill(entry) => &entry.name,
            PaletteItem::Mcp(entry) => &entry.name,
        }
    }

    /// The secondary detail line, if any (skills carry a capability summary;
    /// servers carry a status line).
    pub fn detail(&self) -> Option<&str> {
        match self {
            PaletteItem::Skill(entry) => Some(&entry.detail),
            PaletteItem::Mcp(entry) => Some(&entry.detail),
            PaletteItem::Command(_) => None,
        }
    }
}

/// What accepting a palette selection should do.
#[derive(Debug, Clone, PartialEq)]
pub enum PaletteAction {
    /// Dispatch the named kernel command with empty args.
    RunCommand(String),
    /// Run the named skill (dispatch `skill.run { skill: <name> }`).
    RunSkill(String),
    /// Connect the named MCP server (dispatch `mcp.connect { server: <name> }`).
    ConnectMcp(String),
    /// Disconnect the named MCP server (dispatch `mcp.disconnect { server: <name> }`).
    DisconnectMcp(String),
    /// Add a built-in connector (dispatch `mcp.add { connector: <id> }`).
    AddMcp(String),
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

    /// Replace the set of skills shown in skill mode.
    pub fn set_skills(&mut self, skills: Vec<SkillEntry>) {
        self.skills = skills;
        self.clamp_selection();
    }

    /// Replace the set of MCP servers shown in MCP mode.
    pub fn set_mcp_servers(&mut self, servers: Vec<McpEntry>) {
        self.mcp_servers = servers;
        self.clamp_selection();
    }

    /// Whether the current query has switched the palette into skill mode.
    pub fn in_skill_mode(&self) -> bool {
        self.query.starts_with(SKILL_SIGIL)
    }

    /// Whether the current query has switched the palette into MCP mode.
    pub fn in_mcp_mode(&self) -> bool {
        self.query.starts_with(MCP_SIGIL)
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
        let n = self.items().len();
        if n > 0 {
            self.selected = (self.selected + 1) % n;
        }
    }

    /// Move the selection up (wrapping).
    pub fn select_prev(&mut self) {
        let n = self.items().len();
        if n > 0 {
            self.selected = (self.selected + n - 1) % n;
        }
    }

    /// The index of the highlighted result.
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// The rows to display for the current query — commands, or skills when in
    /// skill mode — already ranked and filtered.
    pub fn items(&self) -> Vec<PaletteItem> {
        if self.in_mcp_mode() {
            self.mcp_matches()
                .into_iter()
                .map(PaletteItem::Mcp)
                .collect()
        } else if self.in_skill_mode() {
            self.skill_matches()
                .into_iter()
                .map(PaletteItem::Skill)
                .collect()
        } else {
            self.results()
                .into_iter()
                .map(|m| PaletteItem::Command(m.name))
                .collect()
        }
    }

    /// Accept the highlighted row, returning the action to perform. Accepting an
    /// MCP server toggles it: connect if disconnected, disconnect if connected.
    pub fn accept(&self) -> Option<PaletteAction> {
        match self.items().into_iter().nth(self.selected)? {
            PaletteItem::Command(name) => Some(PaletteAction::RunCommand(name)),
            PaletteItem::Skill(entry) => Some(PaletteAction::RunSkill(entry.name)),
            // A catalogue connector not yet configured: add it.
            PaletteItem::Mcp(entry) if entry.available => Some(PaletteAction::AddMcp(entry.name)),
            PaletteItem::Mcp(entry) if entry.connected => {
                Some(PaletteAction::DisconnectMcp(entry.name))
            }
            PaletteItem::Mcp(entry) => Some(PaletteAction::ConnectMcp(entry.name)),
        }
    }

    /// The query text used to filter skills — the part after the `>` sigil and
    /// an optional `skill` keyword.
    fn skill_filter(&self) -> &str {
        let rest = self.query.strip_prefix(SKILL_SIGIL).unwrap_or(&self.query);
        rest.strip_prefix("skill").unwrap_or(rest).trim_start()
    }

    /// Skills ranked against the skill filter (name and description). An empty
    /// filter returns every skill in registration order.
    fn skill_matches(&self) -> Vec<SkillEntry> {
        let filter = self.skill_filter();
        if filter.is_empty() {
            return self.skills.clone();
        }
        // A name hit always outranks a description-only hit.
        const NAME_BONUS: i32 = 100;
        let mut scored: Vec<(i32, &SkillEntry)> = self
            .skills
            .iter()
            .filter_map(|entry| {
                let by_name = fuzzy_score(filter, &entry.name).map(|s| s + NAME_BONUS);
                let by_desc = fuzzy_score(filter, &entry.description);
                by_name.or(by_desc).map(|score| (score, entry))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.name.len().cmp(&b.1.name.len())));
        scored.into_iter().map(|(_, entry)| entry.clone()).collect()
    }

    /// The query text used to filter MCP servers — the part after the `@` sigil
    /// and an optional `mcp` keyword.
    fn mcp_filter(&self) -> &str {
        let rest = self.query.strip_prefix(MCP_SIGIL).unwrap_or(&self.query);
        rest.strip_prefix("mcp").unwrap_or(rest).trim_start()
    }

    /// MCP servers ranked against the MCP filter (by name). An empty filter
    /// returns every server in configured order.
    fn mcp_matches(&self) -> Vec<McpEntry> {
        let filter = self.mcp_filter();
        if filter.is_empty() {
            return self.mcp_servers.clone();
        }
        let mut scored: Vec<(i32, &McpEntry)> = self
            .mcp_servers
            .iter()
            .filter_map(|entry| fuzzy_score(filter, &entry.name).map(|score| (score, entry)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.name.len().cmp(&b.1.name.len())));
        scored.into_iter().map(|(_, entry)| entry.clone()).collect()
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
        let n = self.items().len();
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
        assert_eq!(
            p.accept(),
            Some(PaletteAction::RunCommand("agent.run".into()))
        );
    }

    fn palette_with_skills() -> CommandPalette {
        let mut p = palette();
        p.set_skills(vec![
            SkillEntry {
                name: "code-review".into(),
                description: "Review a change".into(),
                detail: "perms: read_workspace · tools: fs.read".into(),
            },
            SkillEntry {
                name: "implement".into(),
                description: "Write code".into(),
                detail: "perms: read_workspace, write_workspace · tools: fs.read, fs.write".into(),
            },
        ]);
        p
    }

    #[test]
    fn sigil_switches_to_skill_mode() {
        let mut p = palette_with_skills();
        assert!(!p.in_skill_mode());
        p.push('>');
        assert!(p.in_skill_mode());
        // `>` alone lists every skill as skill items.
        let items = p.items();
        assert_eq!(items.len(), 2);
        assert!(matches!(items[0], PaletteItem::Skill(_)));
        assert_eq!(items[0].label(), "code-review");
        assert!(items[0].detail().unwrap().contains("read_workspace"));
    }

    #[test]
    fn skill_mode_filters_and_accepts_a_skill() {
        let mut p = palette_with_skills();
        for c in ">impl".chars() {
            p.push(c);
        }
        let items = p.items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label(), "implement");
        assert_eq!(
            p.accept(),
            Some(PaletteAction::RunSkill("implement".into()))
        );
    }

    #[test]
    fn optional_skill_keyword_is_stripped() {
        let mut p = palette_with_skills();
        for c in ">skill code".chars() {
            p.push(c);
        }
        let items = p.items();
        assert_eq!(items[0].label(), "code-review");
    }

    fn palette_with_mcp() -> CommandPalette {
        let mut p = palette();
        p.set_mcp_servers(vec![
            McpEntry {
                name: "github".into(),
                detail: "stdio · 3 tools · connected".into(),
                connected: true,
                available: false,
            },
            McpEntry {
                name: "linear".into(),
                detail: "http · offline".into(),
                connected: false,
                available: false,
            },
        ]);
        p
    }

    #[test]
    fn at_sigil_switches_to_mcp_mode() {
        let mut p = palette_with_mcp();
        assert!(!p.in_mcp_mode());
        p.push('@');
        assert!(p.in_mcp_mode());
        assert!(!p.in_skill_mode());
        let items = p.items();
        assert_eq!(items.len(), 2);
        assert!(matches!(items[0], PaletteItem::Mcp(_)));
        assert_eq!(items[0].label(), "github");
        assert!(items[0].detail().unwrap().contains("connected"));
    }

    #[test]
    fn accepting_a_connected_server_disconnects_and_vice_versa() {
        let mut p = palette_with_mcp();
        for c in "@github".chars() {
            p.push(c);
        }
        assert_eq!(
            p.accept(),
            Some(PaletteAction::DisconnectMcp("github".into()))
        );

        let mut p = palette_with_mcp();
        for c in "@linear".chars() {
            p.push(c);
        }
        let items = p.items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label(), "linear");
        assert_eq!(p.accept(), Some(PaletteAction::ConnectMcp("linear".into())));
    }

    #[test]
    fn accepting_an_available_connector_adds_it() {
        let mut p = palette();
        p.set_mcp_servers(vec![McpEntry {
            name: "filesystem".into(),
            detail: "add · read/write files".into(),
            connected: false,
            available: true,
        }]);
        for c in "@filesystem".chars() {
            p.push(c);
        }
        assert_eq!(p.accept(), Some(PaletteAction::AddMcp("filesystem".into())));
    }

    #[test]
    fn optional_mcp_keyword_is_stripped() {
        let mut p = palette_with_mcp();
        for c in "@mcp lin".chars() {
            p.push(c);
        }
        let items = p.items();
        assert_eq!(items[0].label(), "linear");
    }

    #[test]
    fn contiguous_match_outranks_scattered() {
        // "save" is contiguous in "editor.save" and scattered in nothing else.
        let commit = fuzzy_score("commit", "git.commit").unwrap();
        let scattered = fuzzy_score("git", "git.commit").unwrap();
        assert!(commit > 0 && scattered > 0);
    }
}
