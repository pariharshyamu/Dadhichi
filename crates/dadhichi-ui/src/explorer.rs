//! The file Explorer: a collapsible tree over the workspace.
//!
//! Nodes are built from a directory scan (or an explicit path list, for tests)
//! and flattened to a list of visible rows for rendering. Expanding a directory
//! is a pure state toggle; the tree holds no rendering concerns.

use std::path::{Path, PathBuf};

/// One node in the file tree.
#[derive(Debug, Clone)]
pub struct Node {
    /// Display name (final path component).
    pub name: String,
    /// Full path.
    pub path: PathBuf,
    /// Whether this node is a directory.
    pub is_dir: bool,
    /// Whether an expanded directory's children are shown.
    pub expanded: bool,
    /// Child nodes (directories first, then files, each alphabetical).
    pub children: Vec<Node>,
}

/// A flattened, indented row ready to render.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Indentation depth.
    pub depth: usize,
    /// Display name.
    pub name: String,
    /// Whether it is a directory.
    pub is_dir: bool,
    /// Whether it is expanded (directories only).
    pub expanded: bool,
    /// The node's path.
    pub path: PathBuf,
}

/// A file tree rooted at one directory.
#[derive(Debug)]
pub struct Explorer {
    root: Node,
    selected: usize,
}

impl Explorer {
    /// Scan `root` from the filesystem (one level lazily is fine; here we build
    /// the full tree, skipping hidden entries and `target`).
    pub fn scan(root: impl AsRef<Path>) -> Self {
        let root = build(root.as_ref());
        Self { root, selected: 0 }
    }

    /// Build a tree from an explicit set of relative paths under `root`
    /// (deterministic, for tests).
    pub fn from_paths(root: impl Into<PathBuf>, paths: &[&str]) -> Self {
        let root_path = root.into();
        let name = display_name(&root_path);
        let mut root_node = Node {
            name,
            path: root_path.clone(),
            is_dir: true,
            expanded: true,
            children: Vec::new(),
        };
        for p in paths {
            insert_path(&mut root_node, &root_path, Path::new(p));
        }
        sort_tree(&mut root_node);
        Self {
            root: root_node,
            selected: 0,
        }
    }

    /// Re-scan the tree from the filesystem, picking up files created or removed
    /// since the last scan (e.g. by the agent, or a save-as) while preserving
    /// which directories the user had expanded. The selection is clamped to the
    /// new row count.
    pub fn refresh(&mut self) {
        let mut expanded = Vec::new();
        collect_expanded(&self.root, &mut expanded);
        let mut root = build(&self.root.path);
        for path in &expanded {
            set_expanded(&mut root, path);
        }
        // The root is always shown expanded.
        root.expanded = true;
        self.root = root;
        let rows = self.rows().len();
        if rows > 0 {
            self.selected = self.selected.min(rows - 1);
        } else {
            self.selected = 0;
        }
    }

    /// The root node.
    pub fn root(&self) -> &Node {
        &self.root
    }

    /// The visible rows (respecting collapsed directories).
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        flatten(&self.root, 0, &mut rows);
        rows
    }

    /// The highlighted row index.
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// The path of the highlighted row, if any.
    pub fn selected_path(&self) -> Option<PathBuf> {
        self.rows().get(self.selected).map(|r| r.path.clone())
    }

    /// Move selection down.
    pub fn select_next(&mut self) {
        let n = self.rows().len();
        if n > 0 {
            self.selected = (self.selected + 1).min(n - 1);
        }
    }

    /// Move selection up.
    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Toggle the expanded state of the selected directory.
    pub fn toggle_selected(&mut self) {
        let rows = self.rows();
        let Some(row) = rows.get(self.selected) else {
            return;
        };
        if !row.is_dir {
            return;
        }
        let path = row.path.clone();
        toggle(&mut self.root, &path);
    }
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn build(dir: &Path) -> Node {
    let mut node = Node {
        name: display_name(dir),
        path: dir.to_path_buf(),
        is_dir: true,
        expanded: true,
        children: Vec::new(),
    };
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = display_name(&path);
            if name.starts_with('.') || name == "target" {
                continue;
            }
            if path.is_dir() {
                let mut child = build(&path);
                child.expanded = false; // collapse subdirectories by default
                node.children.push(child);
            } else {
                node.children.push(Node {
                    name,
                    path,
                    is_dir: false,
                    expanded: false,
                    children: Vec::new(),
                });
            }
        }
    }
    sort_tree(&mut node);
    node
}

fn insert_path(parent: &mut Node, base: &Path, rel: &Path) {
    let mut components = rel.components();
    let Some(first) = components.next() else {
        return;
    };
    let seg = first.as_os_str().to_string_lossy().into_owned();
    let child_path = base.join(&seg);
    let rest: PathBuf = components.collect();
    let is_dir = !rest.as_os_str().is_empty();

    let idx = parent.children.iter().position(|c| c.name == seg);
    let child = match idx {
        Some(i) => &mut parent.children[i],
        None => {
            parent.children.push(Node {
                name: seg,
                path: child_path.clone(),
                is_dir,
                expanded: true,
                children: Vec::new(),
            });
            parent.children.last_mut().unwrap()
        }
    };
    if is_dir {
        child.is_dir = true;
        insert_path(child, &child_path, &rest);
    }
}

fn sort_tree(node: &mut Node) {
    node.children
        .sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    for child in &mut node.children {
        sort_tree(child);
    }
}

fn flatten(node: &Node, depth: usize, out: &mut Vec<Row>) {
    // The root itself is shown at depth 0; its children indent from there.
    out.push(Row {
        depth,
        name: node.name.clone(),
        is_dir: node.is_dir,
        expanded: node.expanded,
        path: node.path.clone(),
    });
    if node.is_dir && node.expanded {
        for child in &node.children {
            flatten(child, depth + 1, out);
        }
    }
}

fn toggle(node: &mut Node, path: &Path) -> bool {
    if node.path == path {
        node.expanded = !node.expanded;
        return true;
    }
    for child in &mut node.children {
        if toggle(child, path) {
            return true;
        }
    }
    false
}

/// Gather the paths of every expanded directory (excluding the always-open root)
/// so a re-scan can restore them.
fn collect_expanded(node: &Node, out: &mut Vec<PathBuf>) {
    for child in &node.children {
        if child.is_dir && child.expanded {
            out.push(child.path.clone());
        }
        collect_expanded(child, out);
    }
}

/// Mark the directory at `path` expanded, if it still exists in the tree.
fn set_expanded(node: &mut Node, path: &Path) -> bool {
    if node.path == path {
        node.expanded = true;
        return true;
    }
    for child in &mut node.children {
        if set_expanded(child, path) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_and_flattens_a_tree() {
        let explorer = Explorer::from_paths("/proj", &["src/main.rs", "src/lib.rs", "README.md"]);
        let rows = explorer.rows();
        // root + src (dir) + main.rs + lib.rs + README.md = 5 visible rows.
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].name, "proj");
        // Directories sort before files: `src` precedes `README.md`.
        assert_eq!(rows[1].name, "src");
        assert!(rows[1].is_dir);
    }

    #[test]
    fn refresh_picks_up_new_files_and_keeps_expansion() {
        let dir = std::env::temp_dir().join(format!("dadhichi-explorer-{}", std::process::id()));
        let sub = dir.join("src");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("main.rs"), "fn main() {}").unwrap();

        let mut explorer = Explorer::scan(&dir);
        // Expand `src` (row 1).
        explorer.select_next();
        explorer.toggle_selected();
        assert!(
            explorer.rows().iter().any(|r| r.name == "main.rs"),
            "src expanded before refresh"
        );

        // A new file lands on disk (as the agent would write it).
        std::fs::write(dir.join("NOTES.md"), "hi").unwrap();
        explorer.refresh();

        let names: Vec<_> = explorer.rows().into_iter().map(|r| r.name).collect();
        assert!(names.contains(&"NOTES.md".to_string()), "new file appears");
        assert!(
            names.contains(&"main.rs".to_string()),
            "src stayed expanded across the refresh"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collapsing_a_directory_hides_children() {
        let mut explorer = Explorer::from_paths("/proj", &["src/main.rs", "top.rs"]);
        // Select `src` (row 1) and collapse it.
        explorer.select_next();
        assert_eq!(
            explorer.selected_path().unwrap().file_name().unwrap(),
            "src"
        );
        explorer.toggle_selected();
        let names: Vec<_> = explorer.rows().into_iter().map(|r| r.name).collect();
        assert!(
            !names.contains(&"main.rs".to_string()),
            "collapsed child hidden"
        );
        assert!(names.contains(&"top.rs".to_string()));
    }
}
