use herdr_shepherd::{hooks, trusted};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    wt: PathBuf,
    state: PathBuf,
    config: PathBuf,
}
impl Repo {
    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(&self.root)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self._dir.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().into()
    }
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("main");
        fs::create_dir(&root).unwrap();
        let repo = Self {
            root,
            wt: dir.path().join("ticket"),
            state: dir.path().join("state"),
            config: dir.path().join("config"),
            _dir: dir,
        };
        fs::create_dir(&repo.config).unwrap();
        repo.git(&["init", "-b", "trunk"]);
        repo.git(&["config", "user.name", "Fixture"]);
        repo.git(&["config", "user.email", "fixture@example.invalid"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        fs::write(
            repo.root.join(".shepherd.toml"),
            "[shepherd]\nprotected_branches=['stable']\n",
        )
        .unwrap();
        repo.git(&["add", ".shepherd.toml"]);
        repo.git(&["commit", "-m", "base"]);
        let bare = repo._dir.path().join("origin.git");
        repo.git(&["init", "--bare", bare.to_str().unwrap()]);
        repo.git(&["remote", "add", "origin", bare.to_str().unwrap()]);
        repo.git(&["push", "origin", "trunk"]);
        repo.git(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/trunk",
        ]);
        repo.git(&[
            "worktree",
            "add",
            "-b",
            "feat/x",
            repo.wt.to_str().unwrap(),
            "trunk",
        ]);
        repo
    }
}

#[test]
fn stored_config_blocks_local_default_ref_hook_injection() {
    let repo = Repo::new();
    trusted::default_branch_using(&repo.root, &repo.state, Path::new("/bin/false")).unwrap();
    let marker = repo._dir.path().join("evil-hook-ran");
    fs::write(repo.wt.join(".shepherd.toml"),format!("[shepherd]\nprotected_branches=[]\n[[hooks]]\non='ticket.queued'\ncommand=['touch','{}']\n",marker.display())).unwrap();
    repo.git(&["-C", repo.wt.to_str().unwrap(), "add", ".shepherd.toml"]);
    repo.git(&[
        "-C",
        repo.wt.to_str().unwrap(),
        "commit",
        "-m",
        "evil fixture",
    ]);
    let evil = repo.git(&["-C", repo.wt.to_str().unwrap(), "rev-parse", "HEAD"]);
    repo.git(&[
        "-C",
        repo.wt.to_str().unwrap(),
        "update-ref",
        "refs/heads/trunk",
        &evil,
    ]);
    let cfg = trusted::committed_config(&repo.config, &repo.wt, None, &repo.state).unwrap();
    hooks::emit(&cfg, "ticket.queued", &serde_json::json!({}));
    assert!(
        !marker.exists(),
        "shared local ref injected a privileged hook"
    );
    assert_eq!(
        cfg.raw["shepherd"]["protected_branches"][0].as_str(),
        Some("stable")
    );
}

#[test]
fn snapshot_survives_shared_origin_and_main_head_edits() {
    let repo = Repo::new();
    let before = trusted::ensure_snapshot(&repo.state, &repo.root).unwrap();
    let evil = repo._dir.path().join("evil.git");
    repo.git(&["init", "--bare", evil.to_str().unwrap()]);
    repo.git(&[
        "-C",
        repo.wt.to_str().unwrap(),
        "config",
        "remote.origin.pushurl",
        evil.to_str().unwrap(),
    ]);
    repo.git(&[
        "--git-dir",
        repo.root.join(".git").to_str().unwrap(),
        "symbolic-ref",
        "HEAD",
        "refs/heads/feat/x",
    ]);
    repo.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/feat/x",
    ]);
    let loaded = trusted::load_snapshot(&repo.state, &repo.wt).unwrap();
    assert_eq!(loaded, before);
    assert_eq!(
        trusted::default_branch(&repo.wt, &repo.state)
            .unwrap()
            .as_deref(),
        Some("trunk")
    );
}

#[test]
fn configuration_reads_do_not_create_or_refresh_trust() {
    let repo = Repo::new();
    let cfg = trusted::committed_config(&repo.config, &repo.wt, None, &repo.state).unwrap();
    assert!(!cfg.raw.contains_key("shepherd"));
    assert!(
        trusted::default_branch(&repo.wt, &repo.state)
            .unwrap()
            .is_none()
    );
    assert!(!repo.state.exists());
}

#[test]
fn explicit_refresh_reports_diff_and_replaces_config_bytes() {
    let repo = Repo::new();
    let old = trusted::ensure_snapshot(&repo.state, &repo.root).unwrap();
    fs::write(
        repo.root.join(".shepherd.toml"),
        "[shepherd]\nprotected_branches=['stable','release-safe']\n",
    )
    .unwrap();
    repo.git(&["add", ".shepherd.toml"]);
    repo.git(&["commit", "-m", "user configuration update"]);
    assert_eq!(
        trusted::load_snapshot(&repo.state, &repo.root).unwrap(),
        old
    );
    let refresh = trusted::refresh_snapshot(&repo.state, &repo.root).unwrap();
    assert_eq!(refresh.old, Some(old));
    assert!(
        refresh
            .diff
            .contains("+protected_branches=['stable','release-safe']")
    );
    assert_ne!(
        refresh.new.config_blob,
        refresh.old.as_ref().unwrap().config_blob
    );
    let cfg = trusted::committed_config(&repo.config, &repo.wt, None, &repo.state).unwrap();
    assert_eq!(
        cfg.raw["shepherd"]["protected_branches"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn init_snapshots_its_new_uncommitted_project_config() {
    let repo = Repo::new();
    fs::remove_file(repo.root.join(".shepherd.toml")).unwrap();
    herdr_shepherd::lead::init_with_state(&repo.root, &repo.state).unwrap();
    let trust = trusted::load_snapshot(&repo.state, &repo.root).unwrap();
    assert_eq!(
        trust.config_text.as_deref(),
        Some(herdr_shepherd::lead::DEFAULT_CONFIG)
    );
    assert!(trust.config_blob.is_some());
}

#[test]
fn bootstrap_never_follows_shared_git_config_includes() {
    use std::io::Write;
    let repo = Repo::new();
    let invalid = repo._dir.path().join("not-a-git-config");
    fs::write(&invalid, "[unterminated section\n").unwrap();
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(repo.root.join(".git/config"))
        .unwrap();
    writeln!(file, "\n[include]\npath = {}", invalid.display()).unwrap();
    drop(file);
    let trust = trusted::ensure_snapshot(&repo.state, &repo.root).unwrap();
    assert_eq!(trust.default_branch, "trunk");
    assert!(trust.config_text.as_deref().unwrap().contains("stable"));
}

#[test]
fn credential_bearing_remote_urls_are_not_retained() {
    let repo = Repo::new();
    repo.git(&[
        "remote",
        "set-url",
        "origin",
        "https://fixture-user:fixture-secret@example.invalid/repo",
    ]);
    let error = trusted::ensure_snapshot(&repo.state, &repo.root).unwrap_err();
    assert_eq!(error.code(), "policy_denied");
    assert!(!error.to_string().contains("fixture-secret"));
    assert!(
        herdr_shepherd::store::Store::open(&repo.state, &repo.root)
            .unwrap()
            .read()
            .unwrap()
            .trust
            .is_none()
    );
}

#[test]
fn unrelated_corrupt_and_removed_projects_do_not_block_snapshot_lookup() {
    let repo = Repo::new();
    trusted::ensure_snapshot(&repo.state, &repo.root).unwrap();
    let corrupt = repo.state.join("projects/000-corrupt");
    fs::create_dir_all(&corrupt).unwrap();
    fs::write(corrupt.join("state.json"), "{broken").unwrap();
    let gone = repo._dir.path().join("removed-project");
    fs::create_dir(&gone).unwrap();
    let store = herdr_shepherd::store::Store::open(&repo.state, &gone).unwrap();
    store.update(|_| Ok(())).unwrap();
    fs::remove_dir(gone).unwrap();
    assert_eq!(
        trusted::load_snapshot(&repo.state, &repo.wt)
            .unwrap()
            .default_branch,
        "trunk"
    );
}

#[test]
fn a_corrupt_own_snapshot_fails_closed_instead_of_dropping_protections() {
    let repo = Repo::new();
    trusted::ensure_snapshot(&repo.state, &repo.root).unwrap();
    let store = herdr_shepherd::store::Store::open(&repo.state, &repo.root).unwrap();
    fs::write(store.root().join("state.json"), "{broken").unwrap();
    assert!(trusted::committed_config(&repo.config, &repo.wt, None, &repo.state).is_err());
}

#[test]
fn snapshot_inside_project_state_never_loads_repository_includes() {
    use std::io::Write;
    let mut repo = Repo::new();
    repo.state = repo.root.join("owned-state");
    let invalid = repo._dir.path().join("bad-include");
    fs::write(&invalid, "[unterminated section\n").unwrap();
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(repo.root.join(".git/config"))
        .unwrap();
    writeln!(file, "\n[include]\npath = {}", invalid.display()).unwrap();
    drop(file);
    assert_eq!(
        trusted::ensure_snapshot(&repo.state, &repo.root)
            .unwrap()
            .default_branch,
        "trunk"
    );
}

#[test]
fn trust_probe_uses_explicit_repository() {
    use std::os::unix::fs::PermissionsExt;
    let repo = Repo::new();
    repo.git(&["branch", "stable"]);
    repo.git(&[
        "remote",
        "set-url",
        "origin",
        "https://github.com/fixture/project.git",
    ]);
    let gh = repo._dir.path().join("gh");
    fs::write(&gh, "#!/bin/sh\n[ \"$1\" = repo ] && [ \"$2\" = view ] && [ \"$3\" = https://github.com/fixture/project.git ] || exit 2\nprintf '%s\\n' '{\"defaultBranchRef\":{\"name\":\"stable\"}}'\n").unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o700)).unwrap();
    let branch = trusted::default_branch_using(&repo.root, &repo.state, &gh).unwrap();
    assert_eq!(branch.as_deref(), Some("stable"));
}
