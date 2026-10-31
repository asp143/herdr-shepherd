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
        repo.main.join(".shepherd.toml"),
        "[shepherd]\nprotected_branches = 'feat/t'\n",
    )
    .unwrap();
    ok(&repo.main, &["add", ".shepherd.toml"]);
    ok(&repo.main, &["commit", "-m", "trusted protection fixture"]);
    repo.blocked(&["push", "origin", "feat/t"]);
}

#[test]
fn mixed_protected_branches_array_blocks_direct_push() {
    let repo = Repo::new();
    fs::write(
        repo.main.join(".shepherd.toml"),
        "[shepherd]\nprotected_branches = ['other/*', 1]\n",
    )
    .unwrap();
    ok(&repo.main, &["add", ".shepherd.toml"]);
    ok(&repo.main, &["commit", "-m", "trusted protection fixture"]);
    repo.blocked(&["push", "origin", "feat/t"]);
}

#[test]
fn missing_ticket_marker_fails_closed() {
    let repo = Repo::new();
    ok(
        &repo.worktree,
        &["config", "--worktree", "--unset", "shepherd.ticketBranch"],
    );
    repo.blocked(&["push", "origin", "feat/t"]);
}

#[test]
fn linked_config_redirect_without_marker_is_blocked() {
    let repo = Repo::new();
    let remote = repo._dir.path().join("remote.git");
    let before = ok(
        &repo.main,
        &["--git-dir", remote.to_str().unwrap(), "rev-parse", "main"],
    );
    ok(
        &repo.worktree,
        &["config", "--worktree", "--unset", "shepherd.ticketBranch"],
    );
    ok(
        &repo.worktree,
        &[
            "config",
            "remote.origin.push",
            "+refs/heads/feat/t:refs/heads/main",
        ],
    );
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["--json", "--state-dir"])
        .arg(&repo.state)
        .args(["pr", "push", "--worktree"])
        .arg(&repo.worktree)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        before,
        ok(
            &repo.main,
            &["--git-dir", remote.to_str().unwrap(), "rev-parse", "main"]
        )
    );
    ok(
        &repo.worktree,
        &["config", "--worktree", "--unset", "core.hooksPath"],
    );
    let again = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["--json", "--state-dir"])
        .arg(&repo.state)
        .args(["pr", "push", "--worktree"])
        .arg(&repo.worktree)
        .output()
        .unwrap();
    assert_eq!(again.status.code(), Some(4));
    assert_eq!(
        before,
        ok(
            &repo.main,
            &["--git-dir", remote.to_str().unwrap(), "rev-parse", "main"]
        )
    );
}

#[test]
fn worktree_remote_and_url_redirects_are_ignored() {
    use std::os::unix::fs::PermissionsExt;
    let repo = Repo::new();
    let remote = repo._dir.path().join("remote.git");
    let redirected = repo._dir.path().join("redirected.git");
    ok(
        repo._dir.path(),
        &["init", "--bare", redirected.to_str().unwrap()],
    );
    let before = ok(
        &repo.main,
        &["--git-dir", remote.to_str().unwrap(), "rev-parse", "main"],
    );
    let bin = repo._dir.path().join("fake-bin");
    fs::create_dir(&bin).unwrap();
    fs::write(
        bin.join("gh"),
        "#!/bin/sh\necho 'no pull requests found for branch feat/t' >&2\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(bin.join("gh"), fs::Permissions::from_mode(0o700)).unwrap();
    ok(
        &repo.worktree,
        &[
            "config",
            "--worktree",
            "remote.origin.pushurl",
            redirected.to_str().unwrap(),
        ],
    );
    ok(
        &repo.worktree,
        &[
            "config",
            "--worktree",
            &format!("url.{}.insteadOf", redirected.display()),
            remote.to_str().unwrap(),
        ],
    );
    ok(
        &repo.worktree,
        &["config", "--worktree", "--unset", "core.hooksPath"],
    );
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "PATH",
            std::env::join_paths([bin, "/usr/bin".into(), "/bin".into()]).unwrap(),
        )
        .args(["--json", "--state-dir"])
        .arg(&repo.state)
        .args(["--config-dir"])
        .arg(repo._dir.path().join("cfg"))
        .args(["pr", "push", "--branch", "feat/t", "--worktree"])
        .arg(&repo.worktree)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        before,
        ok(
            &repo.main,
            &["--git-dir", remote.to_str().unwrap(), "rev-parse", "main"]
        )
    );
    assert!(
        ok(
            &repo.main,
            &["--git-dir", redirected.to_str().unwrap(), "for-each-ref"]
        )
        .is_empty()
    );
    assert!(
        !ok(
            &repo.main,
            &["--git-dir", remote.to_str().unwrap(), "rev-parse", "feat/t"]
        )
        .is_empty()
    );
}

#[test]
fn installed_hook_keeps_custom_config_and_state() {
    let repo = Repo::new();
    let cfg = repo._dir.path().join("custom-config");
    fs::create_dir(&cfg).unwrap();
    fs::write(
        cfg.join("shepherd.toml"),
        "[shepherd]\nprotected_branches=['feat/t']\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["--state-dir"])
        .arg(&repo.state)
        .args(["--config-dir"])
        .arg(&cfg)
        .args(["hook", "install", "--branch", "feat/t", "--worktree"])
        .arg(&repo.worktree)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    repo.blocked(&["push", "origin", "feat/t"]);
}
