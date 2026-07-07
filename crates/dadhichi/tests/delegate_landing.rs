//! End-to-end proof of the delegation landing chain: a sub-agent's staged work
//! stays out of the working tree until it is flushed, and then commits to the
//! branch — the mechanism behind `dadhichi delegate`.

use std::sync::Arc;

use dadhichi_git::GitRepo;
use dadhichi_mcp::{OverlayStore, StateStore, WorkspaceStore};

#[test]
fn delegate_work_is_staged_then_lands_and_commits_on_the_branch() {
    let dir = std::env::temp_dir().join(format!("dadhichi-delegate-land-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // A real git repo with an initial commit on the default branch.
    let repo = GitRepo::init(&dir).unwrap();
    std::fs::write(dir.join("README.md"), "# project\n").unwrap();
    repo.stage_all().unwrap();
    repo.commit("init", "tester", "t@example.com").unwrap();

    // The delegate works against a copy-on-write overlay over the workspace.
    let base: Arc<dyn StateStore> = Arc::new(WorkspaceStore::new(&dir));
    let overlay = OverlayStore::new(base.clone());
    overlay
        .write(
            "src/greet.rs",
            "pub fn greet() -> &'static str { \"hi\" }\n",
        )
        .unwrap();
    overlay
        .write("README.md", "# project\n\nNow with a greeter.\n")
        .unwrap();

    // Isolation: the delegate's writes are visible to it but NOT on disk yet.
    assert!(overlay.read("src/greet.rs").unwrap().contains("greet"));
    assert!(!dir.join("src/greet.rs").exists(), "staged, not on disk");
    assert_eq!(
        std::fs::read_to_string(dir.join("README.md")).unwrap(),
        "# project\n",
        "the working tree still has the original README"
    );
    assert_eq!(overlay.changes().len(), 2);

    // Landing flushes the overlay onto the working tree.
    let applied = overlay.flush().unwrap();
    assert_eq!(applied, 2);
    assert!(dir.join("src/greet.rs").exists(), "landed on disk");
    assert!(
        std::fs::read_to_string(dir.join("README.md"))
            .unwrap()
            .contains("greeter")
    );

    // Committing to the branch captures exactly the landed work.
    let repo = GitRepo::open(&dir).unwrap();
    repo.stage_all().unwrap();
    let id = repo
        .commit(
            "code-agent: add a greeter",
            "dadhichi-agent",
            "agent@dadhichi.local",
        )
        .unwrap();
    assert!(!id.is_empty());

    // The tree is clean and the branch now carries the delegate's commit.
    assert!(
        repo.status().unwrap().is_empty(),
        "nothing left uncommitted"
    );
    let log = repo.log(5).unwrap();
    assert_eq!(log.len(), 2);
    assert!(
        log[0].summary.contains("add a greeter"),
        "delegate's commit is on top: {:?}",
        log[0].summary
    );

    let _ = std::fs::remove_dir_all(&dir);
}
