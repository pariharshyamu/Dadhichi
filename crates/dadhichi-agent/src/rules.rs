//! Project rules (`AGENTS.md`) discovery.
//!
//! A project can teach the agent its conventions once, in a Markdown file the
//! agent reads into its system prompt on every run — build commands, style
//! rules, architecture notes — instead of the user restating them each turn.
//! This mirrors the grok-build / Claude Code model.
//!
//! Discovery walks from the repository root down to the working directory, so
//! **deeper files take precedence** (they appear later in the prompt). At each
//! level it loads any recognised top-level rules file plus every `*.md` under a
//! `.dadhichi/rules/` directory. When the working directory is not inside a git
//! repository, only the working directory itself is scanned.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Recognised top-level rules filenames, in load order within a directory.
/// `CLAUDE.md` / `CLAUDE.local.md` are accepted for Claude Code compatibility.
const RULE_FILENAMES: &[&str] = &["AGENTS.md", "AGENT.md", "CLAUDE.md", "CLAUDE.local.md"];

/// One loaded rules file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedRule {
    /// The file it came from.
    pub path: PathBuf,
    /// Its (trimmed) contents.
    pub body: String,
    /// A rough token estimate (~4 chars/token), for `inspect`.
    pub approx_tokens: usize,
}

/// The project rules discovered for a working directory, ordered root → cwd.
#[derive(Debug, Clone, Default)]
pub struct ProjectRules {
    /// The loaded files, lowest-precedence (repo root) first.
    pub files: Vec<LoadedRule>,
}

impl ProjectRules {
    /// Discover the project rules that apply to `cwd`.
    pub fn discover(cwd: &Path) -> ProjectRules {
        let mut files = Vec::new();
        let mut seen = HashSet::new();
        for dir in dir_chain(cwd) {
            for name in RULE_FILENAMES {
                load_if_file(&dir.join(name), &mut files, &mut seen);
            }
            let rules_dir = dir.join(".dadhichi").join("rules");
            if let Ok(entries) = std::fs::read_dir(&rules_dir) {
                let mut md: Vec<PathBuf> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "md"))
                    .collect();
                md.sort();
                for p in md {
                    load_if_file(&p, &mut files, &mut seen);
                }
            }
        }
        ProjectRules { files }
    }

    /// Whether any rules were found.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The total rough token estimate across all loaded files.
    pub fn approx_tokens(&self) -> usize {
        self.files.iter().map(|f| f.approx_tokens).sum()
    }

    /// Render the rules as a system-prompt block, or `None` when there are none.
    /// Files are concatenated root → cwd, each under its path, so deeper (more
    /// specific) instructions appear later and win on conflict.
    pub fn as_prompt_block(&self) -> Option<String> {
        if self.files.is_empty() {
            return None;
        }
        let mut out = String::from(
            "<project_rules>\nProject-specific instructions follow. Treat them as \
             authoritative; when they conflict, the later (more specific) file wins.\n",
        );
        for f in &self.files {
            out.push_str(&format!("\n## {}\n{}\n", f.path.display(), f.body));
        }
        out.push_str("</project_rules>");
        Some(out)
    }
}

/// The directory chain to scan, ordered root → cwd. Inside a git repo this runs
/// from the repository root down to `cwd`; otherwise it is just `cwd`.
fn dir_chain(cwd: &Path) -> Vec<PathBuf> {
    let repo_root = cwd.ancestors().find(|a| a.join(".git").exists());
    let Some(repo_root) = repo_root else {
        return vec![cwd.to_path_buf()];
    };
    let mut chain = Vec::new();
    for a in cwd.ancestors() {
        chain.push(a.to_path_buf());
        if a == repo_root {
            break;
        }
    }
    chain.reverse(); // root … cwd
    chain
}

fn load_if_file(path: &Path, files: &mut Vec<LoadedRule>, seen: &mut HashSet<PathBuf>) {
    if !path.is_file() {
        return;
    }
    // Dedup by canonical path so a case-insensitive filesystem (AGENTS.md ==
    // Agents.md) doesn't load the same file twice.
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !seen.insert(key) {
        return;
    }
    if let Ok(body) = std::fs::read_to_string(path) {
        let body = body.trim().to_string();
        if body.is_empty() {
            return;
        }
        let approx_tokens = body.len().div_ceil(4);
        files.push(LoadedRule {
            path: path.to_path_buf(),
            body,
            approx_tokens,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path, body: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn discovers_nested_rules_root_first() {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        touch(&root.join("AGENTS.md"), "root rule");
        let sub = root.join("crates").join("thing");
        touch(&sub.join("AGENTS.md"), "sub rule");

        let rules = ProjectRules::discover(&sub);
        assert_eq!(rules.files.len(), 2);
        // Root first, deeper last (so deeper wins in-prompt).
        assert_eq!(rules.files[0].body, "root rule");
        assert_eq!(rules.files[1].body, "sub rule");

        let block = rules.as_prompt_block().unwrap();
        assert!(block.contains("<project_rules>"));
        // The deeper rule appears after the root one.
        assert!(block.find("root rule").unwrap() < block.find("sub rule").unwrap());
    }

    #[test]
    fn loads_rules_directory_markdown() {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        touch(&root.join(".dadhichi").join("rules").join("style.md"), "use tabs");

        let rules = ProjectRules::discover(root);
        assert_eq!(rules.files.len(), 1);
        assert_eq!(rules.files[0].body, "use tabs");
        assert!(rules.approx_tokens() > 0);
    }

    #[test]
    fn empty_when_no_rules() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        let rules = ProjectRules::discover(repo.path());
        assert!(rules.is_empty());
        assert!(rules.as_prompt_block().is_none());
    }

    #[test]
    fn skips_empty_files() {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        touch(&root.join("AGENTS.md"), "   \n  ");
        assert!(ProjectRules::discover(root).is_empty());
    }

    #[test]
    fn without_git_only_scans_cwd() {
        // No .git anywhere in the temp tree ⇒ only the cwd is scanned, not
        // ancestors outside the project.
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("AGENTS.md"), "local");
        let rules = ProjectRules::discover(dir.path());
        assert_eq!(rules.files.len(), 1);
        assert_eq!(rules.files[0].body, "local");
    }
}
