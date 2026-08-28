//! Branch names read out of repositories made for the occasion.
//!
//! Every repository here is a real one, made by running the real command in a
//! directory of this test's own. Writing the metadata by hand would have tested
//! this code against this code's own idea of the format, which is exactly the
//! assumption worth checking — so the assumption is checked against the program
//! that defines it, and a machine without that program cannot run these.

use std::fs;
use std::process::Command;

use tempfile::TempDir;

use super::*;

/// The command the repositories here are made with.
const GIT: &str = "git";

/// Who the commits in them are by.
///
/// Named explicitly because the machine running this may have nobody
/// configured, and a commit is refused without one.
const WHO: [&str; 4] = [
    "-c",
    "user.name=Test",
    "-c",
    "user.email=test@example.invalid",
];

/// A directory with nothing in it.
fn dir() -> TempDir {
    tempfile::tempdir().expect("cannot make a directory to work in")
}

/// Runs the command in `at` with `args`, and fails the test if it refuses.
fn git(at: &Path, args: &[&str]) -> String {
    let output = Command::new(GIT)
        .args(WHO)
        .args(args)
        .current_dir(at)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap_or_else(|e| panic!("cannot run `{GIT}`, which these tests need: {e}"));
    assert!(
        output.status.success(),
        "`{GIT} {}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A repository on a branch of the given name, with one commit on it.
fn repository(branch: &str) -> TempDir {
    let dir = dir();
    git(dir.path(), &["init", "--quiet", "--initial-branch", branch]);
    git(
        dir.path(),
        &["commit", "--quiet", "--allow-empty", "-m", "."],
    );
    dir
}

#[test]
fn a_repository_on_a_branch_is_on_that_branch() {
    let repo = repository("work");

    assert_eq!(current_branch(repo.path()).as_deref(), Some("work"));
}

#[test]
fn a_branch_with_nothing_committed_to_it_yet_still_has_a_name() {
    let dir = dir();
    git(dir.path(), &["init", "--quiet", "--initial-branch", "main"]);

    assert_eq!(current_branch(dir.path()).as_deref(), Some("main"));
}

#[test]
fn a_branch_name_with_slashes_in_it_keeps_them() {
    let repo = repository("someone/a-thing");

    assert_eq!(
        current_branch(repo.path()).as_deref(),
        Some("someone/a-thing")
    );
}

#[test]
fn a_tree_on_no_branch_is_the_commit_it_is_on() {
    let repo = repository("main");
    let commit = git(repo.path(), &["rev-parse", "HEAD"]);
    git(repo.path(), &["checkout", "--quiet", "--detach"]);

    let branch = current_branch(repo.path()).expect("a detached head is still an answer");
    assert_eq!(branch, commit[..ABBREVIATED]);
    assert_eq!(branch.len(), ABBREVIATED);
}

#[test]
fn a_folder_below_the_repository_is_on_the_repository_s_branch() {
    let repo = repository("deep");
    let below = repo.path().join("crates").join("thing");
    fs::create_dir_all(&below).unwrap();

    assert_eq!(current_branch(&below).as_deref(), Some("deep"));
}

#[test]
fn an_additional_working_tree_is_on_its_own_branch() {
    let repo = repository("main");
    let beside = dir();
    let tree = beside.path().join("side");
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "sideline",
            tree.to_str().unwrap(),
        ],
    );

    // The arrangement worth covering: the metadata is not beside the tree, it
    // is a file saying where the metadata is.
    assert!(tree.join(METADATA).is_file());
    assert_eq!(current_branch(&tree).as_deref(), Some("sideline"));
}

#[test]
fn a_folder_that_is_not_in_a_repository_has_no_branch() {
    let dir = dir();

    assert_eq!(current_branch(dir.path()), None);
}

#[test]
fn a_folder_that_is_not_there_has_no_branch() {
    let dir = dir();

    assert_eq!(current_branch(&dir.path().join("nowhere")), None);
}

#[test]
fn metadata_that_says_nothing_this_reads_is_no_branch() {
    let dir = dir();
    let metadata = dir.path().join(METADATA);
    fs::create_dir(&metadata).unwrap();
    fs::write(metadata.join(HEAD), "ref: refs/something/else\n").unwrap();

    assert_eq!(current_branch(dir.path()), None);
}

#[test]
fn metadata_naming_somewhere_that_is_not_there_is_no_branch() {
    let dir = dir();
    fs::write(dir.path().join(METADATA), "gitdir: ../nowhere\n").unwrap();

    assert_eq!(current_branch(dir.path()), None);
}

#[test]
fn nothing_that_is_not_a_whole_commit_name_is_abbreviated_into_one() {
    assert_eq!(abbreviated("0123456789abcdef"), Some("0123456".to_owned()));
    assert_eq!(abbreviated("0123456"), Some("0123456".to_owned()));
    assert_eq!(abbreviated("012345"), None);
    assert_eq!(abbreviated("deadbee and then some words"), None);
    assert_eq!(abbreviated(""), None);
}

#[test]
fn a_workspace_is_read_when_it_is_looked_at_and_remembered_after() {
    let repo = repository("held");
    let mut layout = crate::workspace::Layout::new();
    let id = layout.open(repo.path());
    let workspace = layout.workspace(id).expect("just opened");
    let mut branches = Branches::new();

    // Nothing is read until something says the workspace is being looked at.
    assert_eq!(branches.of(id), None);
    assert_eq!(branches.focused(workspace), Some("held"));
    assert_eq!(branches.of(id), Some("held"));

    // What was read stands until the workspace is looked at again, whatever
    // the repository does in the meantime.
    git(repo.path(), &["checkout", "--quiet", "-b", "moved-on"]);
    assert_eq!(branches.of(id), Some("held"));
    assert_eq!(branches.focused(workspace), Some("moved-on"));

    assert!(branches.forget(id));
    assert_eq!(branches.of(id), None);
    assert!(!branches.forget(id));
}

#[test]
fn a_workspace_that_is_not_a_repository_is_remembered_as_having_no_branch() {
    let dir = dir();
    let mut layout = crate::workspace::Layout::new();
    let id = layout.open(dir.path());
    let workspace = layout.workspace(id).expect("just opened");
    let mut branches = Branches::new();

    assert_eq!(branches.focused(workspace), None);
    assert_eq!(branches.of(id), None);
    // Read, and found to have no branch — which is not the same as never having
    // been read, and is why forgetting it has something to forget.
    assert!(branches.forget(id));
}
