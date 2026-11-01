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
    trusted::ensure_snapshot(&repo.state, &repo.root).unwrap();
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
    fs::write(&fake,format!("#!/bin/sh\n[ \"$PWD\" != '{}' ] || exit 7\nprintf '%s\\n' '{{\"defaultBranchRef\":{{\"name\":\"stable\"}}}}'\n",repo.wt.display())).unwrap();
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
    trusted::ensure_snapshot(&repo.state, &repo.root).unwrap();
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
    trusted::ensure_snapshot(&repo.state, &repo.root).unwrap();
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

fn stored_state(repo: &Repo) -> Vec<u8> {
    let root = repo.root.canonicalize().unwrap();
    fs::read(
        repo.state
            .join("projects")
            .join(herdr_shepherd::store::project_hash(&root))
            .join("state.json"),
    )
    .unwrap()
}

fn trust_warnings(report: &trusted::TrustDiff) -> Vec<String> {
    serde_json::to_value(report).unwrap()["warnings"]
        .as_array()
        .expect("trust output includes warnings")
        .iter()
        .map(|warning| warning.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn trust_rejects_untracked_config_without_initializing_snapshot() {
    let repo = Repo::new("");
    fs::write(repo.root.join(".shepherd.toml"), "enabled=false\n").unwrap();
    let error = trusted::refresh_snapshot(&repo.state, &repo.root).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("not committed"), "{message}");
    assert!(message.contains("default branch"), "{message}");
    assert!(message.contains("main"), "{message}");
    assert!(trusted::load_snapshot(&repo.state, &repo.root).is_err());
}

#[test]
fn failed_trust_preserves_snapshot_bytes() {
    let repo = Repo::new("enabled=true\n");
    trusted::ensure_snapshot(&repo.state, &repo.root).unwrap();
    let before = stored_state(&repo);
    repo.git(&["rm", "--cached", ".shepherd.toml"]);
    repo.git(&["commit", "-m", "remove tracked config"]);
    assert!(trusted::refresh_snapshot(&repo.state, &repo.root).is_err());
    assert_eq!(stored_state(&repo), before);
    assert_eq!(
        trusted::load_snapshot(&repo.state, &repo.root)
            .unwrap()
            .config_text
            .as_deref(),
        Some("enabled=true\n")
    );
}

#[test]
fn trust_warns_about_local_edits_and_retains_committed_bytes() {
    let committed = "[shepherd]\nrunner='claude-opus'\n";
    let repo = Repo::new(committed);
    let blob = repo.git(&["rev-parse", "HEAD:.shepherd.toml"]);
    fs::write(
        repo.root.join(".shepherd.toml"),
        "[shepherd]\nrunner='local-edit-marker'\n",
    )
    .unwrap();
    let report = trusted::refresh_snapshot(&repo.state, &repo.root).unwrap();
    assert_eq!(report.new.config_text.as_deref(), Some(committed));
    assert_eq!(report.new.config_blob.as_deref(), Some(blob.as_str()));
    let warnings = trust_warnings(&report);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("not captured"));
    assert!(!warnings[0].contains("local-edit-marker"));
    let cfg = trusted::committed_config(&repo.config, &repo.root, None, &repo.state).unwrap();
    assert_eq!(cfg.raw["shepherd"]["runner"].as_str(), Some("claude-opus"));
}

#[test]
fn trust_matching_config_has_no_warnings() {
    let repo = Repo::new("enabled=true\n");
    let report = trusted::refresh_snapshot(&repo.state, &repo.root).unwrap();
    assert!(trust_warnings(&report).is_empty());
}

#[test]
fn trust_without_local_or_committed_config_succeeds() {
    let repo = Repo::new("");
    let report = trusted::refresh_snapshot(&repo.state, &repo.root).unwrap();
    assert!(report.new.config_text.is_none());
    assert!(report.new.config_blob.is_none());
    assert!(trust_warnings(&report).is_empty());
}

#[test]
fn trust_can_remove_config_after_committed_deletion() {
    let repo = Repo::new("enabled=true\n");
    trusted::ensure_snapshot(&repo.state, &repo.root).unwrap();
    repo.git(&["rm", ".shepherd.toml"]);
    repo.git(&["commit", "-m", "remove project config"]);
    let report = trusted::refresh_snapshot(&repo.state, &repo.root).unwrap();
    assert!(report.new.config_text.is_none());
    assert!(report.new.config_blob.is_none());
    assert!(trust_warnings(&report).is_empty());
}

#[test]
fn trust_does_not_follow_local_config_symlink() {
    let committed = "enabled=true\n";
    let repo = Repo::new(committed);
    fs::remove_file(repo.root.join(".shepherd.toml")).unwrap();
    // Even a symlink to matching bytes cannot establish a local match.
    let target = repo._dir.path().join("local-config-target");
    fs::write(&target, committed).unwrap();
    std::os::unix::fs::symlink(&target, repo.root.join(".shepherd.toml")).unwrap();
    let report = trusted::refresh_snapshot(&repo.state, &repo.root).unwrap();
    assert_eq!(report.new.config_text.as_deref(), Some(committed));
    assert_eq!(trust_warnings(&report).len(), 1);
}

#[test]
fn trust_rejects_dangling_local_symlink_when_no_committed_config() {
    let repo = Repo::new("");
    std::os::unix::fs::symlink(
        repo._dir.path().join("missing-config-target"),
        repo.root.join(".shepherd.toml"),
    )
    .unwrap();
    assert!(trusted::refresh_snapshot(&repo.state, &repo.root).is_err());
    assert!(trusted::load_snapshot(&repo.state, &repo.root).is_err());
}

#[test]
fn trust_uses_committed_bytes_when_local_config_is_not_readable_as_a_file() {
    let repo = Repo::new("enabled=true\n");
    fs::remove_file(repo.root.join(".shepherd.toml")).unwrap();
    fs::create_dir(repo.root.join(".shepherd.toml")).unwrap();
    let report = trusted::refresh_snapshot(&repo.state, &repo.root).unwrap();
    assert_eq!(report.new.config_text.as_deref(), Some("enabled=true\n"));
    assert_eq!(trust_warnings(&report).len(), 1);
}

#[test]
fn init_snapshot_still_accepts_its_explicit_config() {
    let repo = Repo::new("");
    let text = "enabled=false\n";
    fs::write(repo.root.join(".shepherd.toml"), text).unwrap();
    let trust = trusted::snapshot_initialized_config(&repo.state, &repo.root, text).unwrap();
    assert_eq!(trust.config_text.as_deref(), Some(text));
    assert!(trust.config_blob.is_some());
}

#[test]
fn trust_warns_about_edits_in_selected_linked_worktree() {
    let repo = Repo::new("enabled=true\n");
    fs::write(repo.wt.join(".shepherd.toml"), "enabled=false\n").unwrap();
    let report = trusted::refresh_snapshot(&repo.state, &repo.wt).unwrap();
    assert_eq!(report.new.config_text.as_deref(), Some("enabled=true\n"));
    assert_eq!(trust_warnings(&report).len(), 1);
}

#[test]
fn trust_rejects_untracked_config_in_selected_linked_worktree() {
    let repo = Repo::new("");
    trusted::ensure_snapshot(&repo.state, &repo.root).unwrap();
    let before = stored_state(&repo);
    fs::write(repo.wt.join(".shepherd.toml"), "enabled=false\n").unwrap();
    let error = trusted::refresh_snapshot(&repo.state, &repo.wt).unwrap_err();
    assert!(error.to_string().contains("not committed"));
    assert_eq!(stored_state(&repo), before);
}

#[test]
fn drift_comparison_does_not_follow_local_symlink() {
    let repo = Repo::new("enabled=true\n");
    trusted::ensure_snapshot(&repo.state, &repo.root).unwrap();
    let target = repo._dir.path().join("matching-local-config");
    fs::write(&target, "enabled=true\n").unwrap();
    fs::remove_file(repo.root.join(".shepherd.toml")).unwrap();
    std::os::unix::fs::symlink(&target, repo.root.join(".shepherd.toml")).unwrap();
    let warning = trusted::config_drift_warning(&repo.state, &repo.root)
        .unwrap()
        .unwrap();
    assert!(warning.contains("symlinked"));
    assert!(warning.contains("snapshot"));
}

#[test]
fn drift_comparison_does_not_block_on_fifo() {
    let repo = Repo::new("enabled=true\n");
    trusted::ensure_snapshot(&repo.state, &repo.root).unwrap();
    fs::remove_file(repo.root.join(".shepherd.toml")).unwrap();
    assert!(
        repo.command(Path::new("mkfifo"))
            .arg(repo.root.join(".shepherd.toml"))
            .status()
            .unwrap()
            .success()
    );
    let warning = trusted::config_drift_warning(&repo.state, &repo.root)
        .unwrap()
        .unwrap();
    assert!(warning.contains("not a regular file"));
    let report = trusted::refresh_snapshot(&repo.state, &repo.root).unwrap();
    assert_eq!(report.new.config_text.as_deref(), Some("enabled=true\n"));
    assert_eq!(trust_warnings(&report).len(), 1);
}
