//! # dadhichi-git
//!
//! Git integration built on [`git2`] (libgit2). [`GitRepo`] exposes the
//! read/write operations the IDE's Git panel needs — branch, working-tree
//! status, staging, commit, and history — as plain view-model methods. It works
//! entirely on the local repository, needing no network.

use std::path::Path;
use thiserror::Error;

/// Errors from Git operations.
#[derive(Debug, Error)]
pub enum GitError {
    /// The underlying libgit2 call failed.
    #[error("git error: {0}")]
    Git(#[from] git2::Error),
}

/// How a path differs from the last commit / index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    /// New, not yet tracked.
    New,
    /// Tracked and modified.
    Modified,
    /// Deleted.
    Deleted,
    /// Staged in the index.
    Staged,
    /// Renamed.
    Renamed,
}

/// One entry in the working-tree status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    /// Repo-relative path.
    pub path: String,
    /// Its status.
    pub status: FileStatus,
}

/// A commit summary for the history view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    /// Abbreviated commit id.
    pub id: String,
    /// First line of the commit message.
    pub summary: String,
    /// Author name.
    pub author: String,
}

/// A handle to a local Git repository.
pub struct GitRepo {
    repo: git2::Repository,
}

impl std::fmt::Debug for GitRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitRepo")
            .field("path", &self.repo.path())
            .finish()
    }
}

impl GitRepo {
    /// Open an existing repository at or above `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, GitError> {
        Ok(Self {
            repo: git2::Repository::discover(path)?,
        })
    }

    /// Initialise a new repository at `path`.
    pub fn init(path: impl AsRef<Path>) -> Result<Self, GitError> {
        Ok(Self {
            repo: git2::Repository::init(path)?,
        })
    }

    /// The current branch name, or `None` on an unborn/detached HEAD.
    pub fn current_branch(&self) -> Option<String> {
        let head = self.repo.head().ok()?;
        if head.is_branch() {
            head.shorthand().map(str::to_string)
        } else {
            None
        }
    }

    /// The working-tree status (staged and unstaged changes, untracked files).
    pub fn status(&self) -> Result<Vec<StatusEntry>, GitError> {
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true).recurse_untracked_dirs(true);
        let statuses = self.repo.statuses(Some(&mut opts))?;

        let mut out = Vec::new();
        for entry in statuses.iter() {
            let Some(path) = entry.path() else { continue };
            let s = entry.status();
            let status = if s.is_index_new() || s.is_index_modified() {
                FileStatus::Staged
            } else if s.is_wt_new() {
                FileStatus::New
            } else if s.is_wt_modified() {
                FileStatus::Modified
            } else if s.is_wt_deleted() || s.is_index_deleted() {
                FileStatus::Deleted
            } else if s.is_wt_renamed() || s.is_index_renamed() {
                FileStatus::Renamed
            } else {
                continue;
            };
            out.push(StatusEntry {
                path: path.to_string(),
                status,
            });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// Stage every change in the working tree (`git add -A`).
    pub fn stage_all(&self) -> Result<(), GitError> {
        let mut index = self.repo.index()?;
        index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)?;
        index.write()?;
        Ok(())
    }

    /// Commit the staged index with `message`, authored by `name` <`email`>.
    /// Returns the new commit's abbreviated id.
    pub fn commit(&self, message: &str, name: &str, email: &str) -> Result<String, GitError> {
        let sig = git2::Signature::now(name, email)?;
        let mut index = self.repo.index()?;
        let tree_id = index.write_tree()?;
        let tree = self.repo.find_tree(tree_id)?;

        // Parent is the current HEAD commit, if the branch is born.
        let parents = match self.repo.head().ok().and_then(|h| h.target()) {
            Some(oid) => vec![self.repo.find_commit(oid)?],
            None => Vec::new(),
        };
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();

        let oid = self
            .repo
            .commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)?;
        Ok(short_id(oid))
    }

    /// The most recent `limit` commits, newest first.
    pub fn log(&self, limit: usize) -> Result<Vec<CommitInfo>, GitError> {
        let mut revwalk = self.repo.revwalk()?;
        if revwalk.push_head().is_err() {
            return Ok(Vec::new()); // unborn branch, no history yet
        }
        revwalk.set_sorting(git2::Sort::TIME)?;

        let mut out = Vec::new();
        for oid in revwalk.take(limit) {
            let oid = oid?;
            let commit = self.repo.find_commit(oid)?;
            out.push(CommitInfo {
                id: short_id(oid),
                summary: commit.summary().unwrap_or("").to_string(),
                author: commit.author().name().unwrap_or("").to_string(),
            });
        }
        Ok(out)
    }
}

fn short_id(oid: git2::Oid) -> String {
    oid.to_string().chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_with_commit() -> (tempfile::TempDir, GitRepo) {
        let dir = tempfile::tempdir().unwrap();
        let repo = GitRepo::init(dir.path()).unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        repo.stage_all().unwrap();
        repo.commit("initial commit", "Tester", "t@example.com")
            .unwrap();
        (dir, repo)
    }

    #[test]
    fn commits_and_reads_history() {
        let (_dir, repo) = repo_with_commit();
        let log = repo.log(10).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].summary, "initial commit");
        assert_eq!(log[0].author, "Tester");
        assert_eq!(log[0].id.len(), 8);
    }

    #[test]
    fn reports_working_tree_status() {
        let (dir, repo) = repo_with_commit();
        // A brand-new untracked file shows up as New.
        std::fs::write(dir.path().join("b.txt"), "new file").unwrap();
        let status = repo.status().unwrap();
        assert!(
            status
                .iter()
                .any(|e| e.path == "b.txt" && e.status == FileStatus::New)
        );

        // After staging, it becomes Staged.
        repo.stage_all().unwrap();
        let status = repo.status().unwrap();
        assert!(
            status
                .iter()
                .any(|e| e.path == "b.txt" && e.status == FileStatus::Staged)
        );
    }

    #[test]
    fn tracks_current_branch() {
        let (_dir, repo) = repo_with_commit();
        // The default branch is either "master" or "main" depending on config.
        let branch = repo.current_branch().unwrap();
        assert!(branch == "master" || branch == "main", "got: {branch}");
    }
}
