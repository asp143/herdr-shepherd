use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
};

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    remote: PathBuf,
    bin: PathBuf,
    state: PathBuf,
}
impl Repo {
    fn command(&self, program: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self._dir.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .current_dir(&self.root);
        cmd
    }
    fn git(&self, args: &[&str]) -> String {
        let out = self.command("git").args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let remote = dir.path().join("remote.git");
        let bin = dir.path().join("bin");
        let state = dir.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&bin).unwrap();
        let repo = Self {
            _dir: dir,
            root,
            remote,
            bin,
            state,
        };
        repo.git(&["init", "-b", "feat/t"]);
        repo.git(&["config", "user.name", "Fixture"]);
        repo.git(&["config", "user.email", "fixture@example.invalid"]);
        repo.git(&["commit", "--allow-empty", "-m", "fixture"]);
        repo.git(&["tag", "-a", "existing", "-m", "existing fixture"]);
        repo.git(&["init", "--bare", repo.remote.to_str().unwrap()]);
        repo.git(&["remote", "add", "origin", repo.remote.to_str().unwrap()]);
        repo.git(&["push", "origin", "HEAD:main", "refs/tags/existing"]);
        fs::write(
            repo.bin.join("gh"),
            "#!/bin/sh\nprintf '%s\\n' 'no pull requests found for branch feat/t' >&2\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(repo.bin.join("gh"), fs::Permissions::from_mode(0o700)).unwrap();
        let store = herdr_shepherd::store::Store::open(&repo.state, &repo.root).unwrap();
        store
            .update(|data| {
                data.tickets.push(herdr_shepherd::store::Ticket {
                    id: "T".into(),
                    branch: "feat/t".into(),
                    base: "main".into(),
                    worktree: Some(repo.root.clone()),
                    ..Default::default()
                });
                Ok(())
            })
            .unwrap();
        let file = store.root().join("state.json");
        let mut data: serde_json::Value =
            serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        data["trust"] = serde_json::json!({"project_root":repo.root,"common_git_dir":repo.root.join(".git"),"origin_url":repo.remote.to_str().unwrap(),"push_url":repo.remote.to_str().unwrap(),"default_branch":"main","config_commit":null,"config_blob":null,"config_text":null,"trusted_at":"fixture","global_config":"/dev/null","object_format":"sha1"});
        fs::write(file, serde_json::to_vec(&data).unwrap()).unwrap();
        repo
    }
    fn push(&self) -> Output {
        let path =
            std::env::join_paths([self.bin.clone(), "/usr/bin".into(), "/bin".into()]).unwrap();
        self.command(env!("CARGO_BIN_EXE_herdr-shepherd"))
            .env("PATH", path)
            .env("HERDR_PLUGIN_CONFIG_DIR", self._dir.path().join("config"))
            .env("SHEPHERD_TEST_ALLOW_LOCAL_ORIGIN", "allow-local-in-tmp")
            .arg("--state-dir")
            .arg(&self.state)
            .args(["--json", "pr", "push", "--branch", "feat/t", "--worktree"])
            .arg(&self.root)
            .output()
            .unwrap()
    }
    fn remote_refs(&self) -> Vec<String> {
        self.git(&[
            "--git-dir",
            self.remote.to_str().unwrap(),
            "for-each-ref",
            "--format=%(refname)",
        ])
        .lines()
        .map(str::to_owned)
        .collect()
    }
}
#[test]
fn push_disables_implicit_follow_tags() {
    let repo = Repo::new();
    repo.git(&["tag", "-a", "unrequested", "-m", "unrequested fixture"]);
    repo.git(&["config", "push.followTags", "true"]);
    let out = repo.push();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        repo.remote_refs(),
        ["refs/heads/feat/t", "refs/heads/main", "refs/tags/existing"]
    );
}
#[test]
fn push_disables_remote_mirror_and_preserves_refs() {
    let repo = Repo::new();
    repo.git(&["tag", "-a", "unrequested", "-m", "unrequested fixture"]);
    repo.git(&["config", "remote.origin.mirror", "true"]);
    let out = repo.push();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        repo.remote_refs(),
        ["refs/heads/feat/t", "refs/heads/main", "refs/tags/existing"]
    );
}

#[test]
fn config_redirect_without_explicit_ticket_is_blocked() {
    let repo = Repo::new();
    herdr_shepherd::store::Store::open(&repo.state, &repo.root)
        .unwrap()
        .update(|data| {
            data.tickets.clear();
            Ok(())
        })
        .unwrap();
    let before = repo.git(&[
        "--git-dir",
        repo.remote.to_str().unwrap(),
        "rev-parse",
        "main",
    ]);
    repo.git(&[
        "config",
        "remote.origin.push",
        "+refs/heads/feat/t:refs/heads/main",
    ]);
    repo.git(&["commit", "--allow-empty", "-m", "attacker"]);
    let out = repo
        .command(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .env(
            "PATH",
            std::env::join_paths([repo.bin.clone(), "/usr/bin".into(), "/bin".into()]).unwrap(),
        )
        .env("HERDR_PLUGIN_CONFIG_DIR", repo._dir.path().join("config"))
        .arg("--state-dir")
        .arg(&repo.state)
        .args(["--json", "pr", "push", "--worktree"])
        .arg(&repo.root)
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
        repo.git(&[
            "--git-dir",
            repo.remote.to_str().unwrap(),
            "rev-parse",
            "main"
        ])
    );
}

#[test]
fn config_redirect_cannot_rewrite_main_with_explicit_branch() {
    let repo = Repo::new();
    let before = repo.git(&[
        "--git-dir",
        repo.remote.to_str().unwrap(),
        "rev-parse",
        "main",
    ]);
    repo.git(&[
        "config",
        "remote.origin.push",
        "+refs/heads/feat/t:refs/heads/main",
    ]);
    repo.git(&["config", "core.hooksPath", "/dev/null"]);
    repo.git(&["commit", "--allow-empty", "-m", "ticket"]);
    let out = repo.push();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        before,
        repo.git(&[
            "--git-dir",
            repo.remote.to_str().unwrap(),
            "rev-parse",
            "main"
        ])
    );
    assert!(repo.remote_refs().contains(&"refs/heads/feat/t".to_owned()));
}
