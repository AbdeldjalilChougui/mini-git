//! Integration tests: objects written by `mgit` must be readable by real
//! `git`, and objects written by `git` must be readable by `mgit`.
//! Skipped (with a message) when `git` is not on PATH.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn have_git() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

fn mgit(dir: &Path, args: &[&str]) -> Output {
    let out = Command::new(env!("CARGO_BIN_EXE_mgit"))
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("run mgit");
    assert!(
        out.status.success(),
        "mgit {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn git(dir: &Path, args: &[&str]) -> Output {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn stdout(o: Output) -> String {
    String::from_utf8(o.stdout).unwrap().trim_end().to_string()
}

fn setup_tree(dir: &Path) {
    fs::write(dir.join("README.md"), "# hello\n").unwrap();
    fs::create_dir_all(dir.join("src/nested")).unwrap();
    fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(dir.join("src/nested/a.txt"), "a\n").unwrap();
    fs::write(dir.join("src.txt"), "sorts before src/ in git order\n").unwrap();
}

#[test]
fn hash_object_matches_git() {
    if !have_git() {
        eprintln!("git not found; skipping");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    mgit(t.path(), &["init"]);
    fs::write(t.path().join("f.txt"), "some content\n").unwrap();
    let ours = stdout(mgit(t.path(), &["hash-object", "-w", "f.txt"]));
    let theirs = stdout(git(t.path(), &["hash-object", "f.txt"]));
    assert_eq!(ours, theirs);
    assert_eq!(stdout(git(t.path(), &["cat-file", "-t", &ours])), "blob");
    assert_eq!(stdout(git(t.path(), &["cat-file", "-p", &ours])), "some content");
}

#[test]
fn write_tree_matches_git_add_write_tree() {
    if !have_git() {
        return;
    }
    let t = tempfile::tempdir().unwrap();
    mgit(t.path(), &["init"]);
    setup_tree(t.path());
    let ours = stdout(mgit(t.path(), &["write-tree"]));
    git(t.path(), &["add", "-A"]);
    let theirs = stdout(git(t.path(), &["write-tree"]));
    assert_eq!(ours, theirs);
}

#[test]
fn commits_pass_git_fsck_and_log() {
    if !have_git() {
        return;
    }
    let t = tempfile::tempdir().unwrap();
    mgit(t.path(), &["init"]);
    setup_tree(t.path());
    mgit(t.path(), &["commit", "-m", "first commit"]);
    fs::write(t.path().join("README.md"), "# hello again\n").unwrap();
    mgit(t.path(), &["commit", "-m", "second commit"]);

    git(t.path(), &["fsck", "--strict", "--no-dangling"]);
    let log = stdout(git(t.path(), &["log", "--format=%s"]));
    assert_eq!(log, "second commit\nfirst commit");
    let head_git = stdout(git(t.path(), &["rev-parse", "HEAD"]));
    let head_ours = stdout(mgit(t.path(), &["rev-parse", "HEAD"]));
    assert_eq!(head_git, head_ours);
    let show = stdout(git(t.path(), &["show", "HEAD:README.md"]));
    assert_eq!(show, "# hello again");
}

#[test]
fn commit_tree_with_parent_is_valid() {
    if !have_git() {
        return;
    }
    let t = tempfile::tempdir().unwrap();
    mgit(t.path(), &["init"]);
    setup_tree(t.path());
    let tree = stdout(mgit(t.path(), &["write-tree"]));
    let c1 = stdout(mgit(t.path(), &["commit-tree", &tree, "-m", "root"]));
    let c2 = stdout(mgit(t.path(), &["commit-tree", &tree, "-p", &c1, "-m", "child"]));
    assert_eq!(stdout(git(t.path(), &["cat-file", "-t", &c2])), "commit");
    assert_eq!(stdout(git(t.path(), &["rev-parse", &format!("{c2}^")])), c1);
    assert_eq!(stdout(git(t.path(), &["rev-parse", &format!("{c2}^{{tree}}")])), tree);
}

#[test]
fn mgit_reads_objects_written_by_git() {
    if !have_git() {
        return;
    }
    let t = tempfile::tempdir().unwrap();
    git(t.path(), &["init", "-q", "-b", "main"]);
    setup_tree(t.path());
    git(t.path(), &["add", "-A"]);
    git(t.path(), &["commit", "-q", "-m", "made by git"]);
    let head = stdout(git(t.path(), &["rev-parse", "HEAD"]));
    assert_eq!(stdout(mgit(t.path(), &["rev-parse", "HEAD"])), head);
    assert_eq!(stdout(mgit(t.path(), &["cat-file", "-t", &head])), "commit");
    let log = stdout(mgit(t.path(), &["log"]));
    assert!(log.contains("made by git"), "{log}");
    let tree = stdout(git(t.path(), &["rev-parse", "HEAD^{tree}"]));
    assert_eq!(
        stdout(mgit(t.path(), &["cat-file", "-p", &tree])),
        stdout(git(t.path(), &["cat-file", "-p", &tree]))
    );
}

#[test]
fn checkout_switches_branches_and_git_agrees() {
    if !have_git() {
        return;
    }
    let t = tempfile::tempdir().unwrap();
    mgit(t.path(), &["init"]);
    setup_tree(t.path());
    mgit(t.path(), &["commit", "-m", "base"]);
    mgit(t.path(), &["branch", "feature"]);
    mgit(t.path(), &["checkout", "feature"]);
    fs::write(t.path().join("feature.txt"), "new\n").unwrap();
    mgit(t.path(), &["commit", "-m", "feature work"]);
    let main = stdout(git(t.path(), &["symbolic-ref", "HEAD"]));
    assert_eq!(main, "refs/heads/feature");

    let branches = stdout(mgit(t.path(), &["branch"]));
    let other = branches
        .lines()
        .map(|l| l.trim_start_matches(['*', ' ']))
        .find(|b| *b != "feature")
        .unwrap()
        .to_string();
    mgit(t.path(), &["checkout", &other]);
    assert!(!t.path().join("feature.txt").exists());
    assert_eq!(stdout(git(t.path(), &["symbolic-ref", "HEAD"])), format!("refs/heads/{other}"));
    mgit(t.path(), &["checkout", "feature"]);
    assert_eq!(fs::read_to_string(t.path().join("feature.txt")).unwrap(), "new\n");
}
