use herdr_shepherd::{hooks, trusted};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
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
    fn command(&self, program: &Path) -> Command {
        let mut cmd = Command::new(program);
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self._dir.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .current_dir(&self.root);
        cmd
    }
    fn git(&self, args: &[&str]) -> String {
        let out = self.command(Path::new("git")).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    fn new(config: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir(&root).unwrap();
        let repo = Self {
            root,
            wt: dir.path().join("ticket"),
            state: dir.path().join("state"),
            config: dir.path().join("config"),
            _dir: dir,
        };
        fs::create_dir(&repo.config).unwrap();
        repo.git(&["init", "-b", "main"]);
        repo.git(&["config", "user.name", "Fixture"]);
        repo.git(&["config", "user.email", "fixture@example.invalid"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        if !config.is_empty() {
            fs::write(repo.root.join(".shepherd.toml"), config).unwrap();
            repo.git(&["add", ".shepherd.toml"]);
        }
        repo.git(&["commit", "--allow-empty", "-m", "base"]);
        let bare = repo._dir.path().join("remote.git");
        repo.git(&["init", "--bare", bare.to_str().unwrap()]);
        repo.git(&["remote", "add", "origin", bare.to_str().unwrap()]);
        repo.git(&["push", "origin", "main"]);
        repo.git(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ]);
        repo.git(&[
            "worktree",
            "add",
            "-b",
            "feat/t",
            repo.wt.to_str().unwrap(),
            "main",
        ]);
        repo
    }
}

#[test]
fn committed_base_blocks_ticket_config_hook_injection() {
    let repo = Repo::new("[shepherd]\nprotected_branches=['stable']\n");
    let marker = repo._dir.path().join("injected");
    let evil = format!(
        "[shepherd]\nprotected_branches=[]\n[[hooks]]\non='ticket.queued'\ncommand=['touch','{}']\n",
        marker.display()
    );
    fs::write(repo.wt.join(".shepherd.toml"), &evil).unwrap();
    fs::write(repo.root.join(".shepherd.toml"), &evil).unwrap();
    let cfg = trusted::committed_config(&repo.config, &repo.wt, Some("main"), &repo.state).unwrap();
    assert_eq!(
        cfg.raw["shepherd"]["protected_branches"][0].as_str(),
        Some("stable")
    );
    hooks::emit(&cfg, "ticket.queued", &serde_json::json!({}));
    assert!(!marker.exists());
    assert_eq!(
        trusted::main_checkout(&repo.wt).unwrap(),
        repo.root.canonicalize().unwrap()
    );
}

#[test]
fn cached_provider_default_survives_mutable_origin_head() {
    let repo = Repo::new("[shepherd]\nprotected_branches=['stable']\n");
    repo.git(&["branch", "stable"]);
    repo.git(&[
        "remote",
        "set-url",
        "origin",
        "https://github.com/fixture/repository",
    ]);
    let fake = repo._dir.path().join("gh");
    fs::write(&fake,format!("#!/bin/sh\n[ \"$PWD\" = '{}' ] || exit 7\nprintf '%s\\n' '{{\"defaultBranchRef\":{{\"name\":\"stable\"}}}}'\n",repo.root.display())).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        trusted::default_branch_using(&repo.wt, &repo.state, &fake)
            .unwrap()
            .as_deref(),
        Some("stable")
    );
    repo.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/feat/t",
    ]);
    fs::write(&fake, "#!/bin/sh\nexit 9\n").unwrap();
    assert_eq!(
        trusted::default_branch(&repo.wt, &repo.state)
            .unwrap()
            .as_deref(),
        Some("stable")
    );
    repo.git(&["config", "extensions.worktreeConfig", "true"]);
    repo.git(&[
        "-C",
        repo.wt.to_str().unwrap(),
        "config",
        "--worktree",
        "remote.origin.url",
        "https://github.com/attacker/redirect",
    ]);
    let cfg =
        trusted::committed_config(&repo.config, &repo.wt, Some("feat/t"), &repo.state).unwrap();
    assert_eq!(
        cfg.raw["shepherd"]["protected_branches"][0].as_str(),
        Some("stable")
    );
}

#[test]
fn missing_committed_config_retains_global_hooks() {
    let repo = Repo::new("");
    let good = repo._dir.path().join("global-hook");
    fs::write(
        repo.config.join("shepherd.toml"),
        format!(
            "[[hooks]]\non='ticket.queued'\ncommand=['touch','{}']\n",
            good.display()
        ),
    )
    .unwrap();
    fs::write(repo.wt.join(".shepherd.toml"), "enabled=false\n").unwrap();
    let cfg = trusted::committed_config(&repo.config, &repo.wt, None, &repo.state).unwrap();
    assert!(cfg.enabled);
    hooks::emit(&cfg, "ticket.queued", &serde_json::json!({}));
    assert!(good.exists());
}

#[test]
fn committed_project_hooks_remain_available() {
    let repo = Repo::new("enabled=true\n");
    let good = repo._dir.path().join("committed-hook");
    fs::write(
        repo.root.join(".shepherd.toml"),
        format!(
            "[[hooks]]\non='ticket.queued'\ncommand=['touch','{}']\n",
            good.display()
        ),
    )
    .unwrap();
    repo.git(&["add", ".shepherd.toml"]);
    repo.git(&["commit", "-m", "trusted hook"]);
    repo.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let cfg = trusted::committed_config(&repo.config, &repo.wt, None, &repo.state).unwrap();
    hooks::emit(&cfg, "ticket.queued", &serde_json::json!({}));
    assert!(good.exists());
}

#[test]
fn global_only_hooks_from_nonrepo() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    fs::create_dir(&config).unwrap();
    let marker = dir.path().join("global-ran");
    fs::write(
        config.join("shepherd.toml"),
        format!(
            "[[hooks]]\non='ticket.queued'\ncommand=['touch','{}']\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::write(dir.path().join(".shepherd.toml"), "enabled=false\n").unwrap();
    let cfg =
        trusted::committed_config(&config, dir.path(), None, &dir.path().join("state")).unwrap();
    assert!(cfg.enabled);
    hooks::emit(&cfg, "ticket.queued", &serde_json::json!({}));
    assert!(marker.exists());
}

#[test]
fn failed_provider_ignores_poisoned_shared_origin_head() {
    let repo = Repo::new("enabled=true\n");
    repo.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/feat/t",
    ]);
    let fake = repo._dir.path().join("gh-failed");
    fs::write(&fake, "#!/bin/sh\nexit 9\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        trusted::default_branch_using(&repo.wt, &repo.state, &fake)
            .unwrap()
            .as_deref(),
        Some("main")
    );
    assert_eq!(
        trusted::default_branch(&repo.wt, &repo.state)
            .unwrap()
            .as_deref(),
        Some("main")
    );
}

#[test]
fn poisoned_remote_tracking_config_cannot_inject_hooks() {
    let repo = Repo::new("[shepherd]\nprotected_branches=['stable']\n");
    let trusted_main = repo.git(&["rev-parse", "refs/heads/main"]);
    repo.git(&[
        "remote",
        "set-url",
        "origin",
        "https://github.com/fixture/repository",
    ]);
    let fake = repo._dir.path().join("gh-main");
    fs::write(
        &fake,
        "#!/bin/sh\nprintf '%s\\n' '{\"defaultBranchRef\":{\"name\":\"main\"}}'\n",
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    trusted::default_branch_using(&repo.wt, &repo.state, &fake).unwrap();
    let marker = repo._dir.path().join("poisoned-ref-hook");
    fs::write(
        repo.wt.join(".shepherd.toml"),
        format!(
            "[[hooks]]\non='ticket.queued'\ncommand=['touch','{}']\n",
            marker.display()
        ),
    )
    .unwrap();
    repo.git(&["-C", repo.wt.to_str().unwrap(), "add", ".shepherd.toml"]);
    repo.git(&[
        "-C",
        repo.wt.to_str().unwrap(),
        "commit",
        "-m",
        "poison fixture",
    ]);
    let malicious = repo.git(&["-C", repo.wt.to_str().unwrap(), "rev-parse", "HEAD"]);
    repo.git(&["update-ref", "refs/remotes/origin/main", &malicious]);
    let cfg = trusted::committed_config(&repo.config, &repo.wt, None, &repo.state).unwrap();
    hooks::emit(&cfg, "ticket.queued", &serde_json::json!({}));
    assert!(
        !marker.exists(),
        "an editable shared remote-tracking ref supplied a hook"
    );
    assert_eq!(
        cfg.raw["shepherd"]["protected_branches"][0].as_str(),
        Some("stable")
    );
    assert_eq!(repo.git(&["rev-parse", "refs/heads/main"]), trusted_main);
}
