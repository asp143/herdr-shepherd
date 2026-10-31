use herdr_shepherd::gate::hook::install;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn git(path: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(path)
        .args(args)
        .output()
        .unwrap()
}
fn ok(path: &Path, args: &[&str]) -> String {
    let output = git(path, args);
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
struct Repo {
    _dir: tempfile::TempDir,
    main: PathBuf,
    worktree: PathBuf,
    state: PathBuf,
}
impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        let remote = dir.path().join("remote.git");
        let worktree = dir.path().join("ticket");
        let state = dir.path().join("state");
        fs::create_dir(&main).unwrap();
        ok(&main, &["init", "-b", "main"]);
        ok(&main, &["config", "user.email", "fixture@example.invalid"]);
        ok(&main, &["config", "user.name", "Fixture"]);
        fs::write(main.join("file"), "base\n").unwrap();
        ok(&main, &["add", "file"]);
        ok(&main, &["commit", "-m", "base"]);
        ok(dir.path(), &["init", "--bare", remote.to_str().unwrap()]);
        ok(
            &main,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        ok(&main, &["push", "origin", "main"]);
        ok(
            &main,
            &[
                "worktree",
                "add",
                "-b",
                "feat/t",
                worktree.to_str().unwrap(),
            ],
        );
        fs::write(worktree.join("file"), "ticket\n").unwrap();
        ok(&worktree, &["commit", "-am", "ticket"]);
        install(
            &worktree,
            "feat/t",
            &state,
            Path::new(env!("CARGO_BIN_EXE_herdr-shepherd")),
        )
        .unwrap();
        Self {
            _dir: dir,
            main,
            worktree,
            state,
        }
    }
    fn push(&self) {
        ok(&self.worktree, &["push", "origin", "feat/t"]);
    }
    fn blocked(&self, args: &[&str]) {
        let output = git(&self.worktree, args);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("push blocked"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
#[test]
fn allows_ticket_branch_push() {
    Repo::new().push();
}
#[test]
fn blocks_push_to_main() {
    Repo::new().blocked(&["push", "origin", "HEAD:main"]);
}
#[test]
fn blocks_force_push() {
    let repo = Repo::new();
    repo.push();
    fs::write(repo.worktree.join("file"), "rewritten\n").unwrap();
    ok(&repo.worktree, &["commit", "-am", "rewrite", "--amend"]);
    repo.blocked(&["push", "-f", "origin", "feat/t"]);
}
#[test]
fn blocks_delete() {
    let repo = Repo::new();
    repo.push();
    repo.blocked(&["push", "origin", ":feat/t"]);
}
#[test]
fn other_worktree_branch_blocked() {
    let repo = Repo::new();
    let other = repo._dir.path().join("other");
    ok(
        &repo.main,
        &["worktree", "add", "-b", "feat/u", other.to_str().unwrap()],
    );
    install(
        &other,
        "feat/u",
        &repo.state,
        Path::new(env!("CARGO_BIN_EXE_herdr-shepherd")),
    )
    .unwrap();
    let output = git(&other, &["push", "origin", "feat/t"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("push blocked"));
}
#[test]
fn main_checkout_unaffected() {
    let repo = Repo::new();
    assert!(
        git(&repo.main, &["config", "--get", "core.hooksPath"])
            .stdout
            .is_empty()
    );
    fs::write(repo.main.join("file"), "main update\n").unwrap();
    ok(&repo.main, &["commit", "-am", "main update"]);
    ok(&repo.main, &["push", "origin", "main"]);
}

#[test]
fn main_checkout_install_refused() {
    let repo = Repo::new();
    assert!(
        install(
            &repo.main,
            "main",
            &repo.state,
            Path::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        )
        .is_err()
    );
    assert!(
        git(&repo.main, &["config", "--get", "core.hooksPath"])
            .stdout
            .is_empty()
    );
}

#[test]
fn invalid_protected_branches_type_blocks_direct_push() {
    let repo = Repo::new();
    fs::write(
        repo.worktree.join(".shepherd.toml"),
        "[shepherd]\nprotected_branches = 'feat/t'\n",
    )
    .unwrap();
    repo.blocked(&["push", "origin", "feat/t"]);
}

#[test]
fn mixed_protected_branches_array_blocks_direct_push() {
    let repo = Repo::new();
    fs::write(
        repo.worktree.join(".shepherd.toml"),
        "[shepherd]\nprotected_branches = ['other/*', 1]\n",
    )
    .unwrap();
    repo.blocked(&["push", "origin", "feat/t"]);
}
