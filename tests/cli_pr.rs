use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn git(path: &Path, args: &[&str]) -> Output {
    let out = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn repo(path: &Path, branch: &str) {
    git(path, &["init", "--quiet", "--initial-branch", branch]);
    git(
        path,
        &[
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "user.name=Fixture",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    );
}

#[test]
fn view_without_gh_is_github_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    fs::create_dir(&repo_dir).unwrap();
    repo(&repo_dir, "feat/test");
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let git_bin = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    std::os::unix::fs::symlink(
        String::from_utf8(git_bin.stdout).unwrap().trim(),
        bin.join("git"),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("PATH", &bin)
        .env("HERDR_PLUGIN_CONFIG_DIR", dir.path().join("config"))
        .args(["--json", "pr", "view", "--worktree"])
        .arg(&repo_dir)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["error"]["code"], "github_unavailable");
}

#[test]
fn pr_push_to_protected_denied() {
    let dir = tempfile::tempdir().unwrap();
    repo(dir.path(), "main");
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HERDR_PLUGIN_CONFIG_DIR", dir.path().join("config"))
        .args(["--json", "pr", "push", "--worktree"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["error"]["code"], "policy_denied");
}

struct FakeGh {
    _dir: tempfile::TempDir,
    repo: std::path::PathBuf,
    bin: std::path::PathBuf,
    args: std::path::PathBuf,
    input: std::path::PathBuf,
    body: std::path::PathBuf,
    config: std::path::PathBuf,
}

impl FakeGh {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = dir.path().join("repo");
        fs::create_dir(&repo_dir).unwrap();
        repo(&repo_dir, "feat/test");
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let git_bin = Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap();
        std::os::unix::fs::symlink(
            String::from_utf8(git_bin.stdout).unwrap().trim(),
            bin.join("git"),
        )
        .unwrap();
        fs::write(bin.join("gh"), r#"#!/bin/sh
printf 'CALL\n' >> "$SHEPHERD_TEST_ARGS"
printf '%s\n' "$@" >> "$SHEPHERD_TEST_ARGS"
if [ -n "$SHEPHERD_TEST_ERROR" ]; then
    printf '%s\n' "$SHEPHERD_TEST_ERROR" >&2
    exit 1
fi
case "$1 $2" in
  'pr create') /bin/cat > "$SHEPHERD_TEST_INPUT"; printf '%s\n' 'https://github.com/example/project/pull/42' ;;
  'pr view') /bin/cat "$SHEPHERD_TEST_FIXTURE" ;;
  'repo view') printf '%s\n' 'example/repo' ;;
  'pr comment') /bin/cat > "$SHEPHERD_TEST_INPUT" ;;
  'api user') printf '%s\n' 'fixture-author' ;;
  *) printf '%s\n' '{}' ;;
esac
"#).unwrap();
        fs::set_permissions(bin.join("gh"), fs::Permissions::from_mode(0o700)).unwrap();
        let args = dir.path().join("args.txt");
        let input = dir.path().join("input.txt");
        let body = dir.path().join("body.txt");
        let config = dir.path().join("config");
        let state = dir.path().join("state");
        let store = herdr_shepherd::store::Store::open(&state, &repo_dir).unwrap();
        let remote = dir.path().join("remote.git");
        git(&repo_dir, &["branch", "main"]);
        git(
            &repo_dir,
            &[
                "clone",
                "--bare",
                "--quiet",
                repo_dir.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );
        store
            .update(|data| {
                data.trust = Some(herdr_shepherd::store::ProjectTrust {
                    project_root: repo_dir.clone(),
                    common_git_dir: repo_dir.join(".git"),
                    origin_url: "https://github.com/example/repo.git".into(),
                    push_url: remote.to_string_lossy().into_owned(),
                    default_branch: "main".into(),
                    config_commit: None,
                    config_blob: None,
                    config_text: None,
                    trusted_at: "fixture".into(),
                    global_config: None,
                    object_format: "sha1".into(),
                });
                data.tickets.push(herdr_shepherd::store::Ticket {
                    id: "T1".into(),
                    branch: "feat/test".into(),
                    base: "main".into(),
                    worktree: Some(repo_dir.clone()),
                    ..Default::default()
                });
                Ok(())
            })
            .unwrap();

        fs::write(
            &body,
            "A literal reply: $(unchanged) 'quoted'\nsecond line\n",
        )
        .unwrap();
        Self {
            _dir: dir,
            repo: repo_dir,
            bin,
            args,
            input,
            body,
            config,
        }
    }

    fn command(&self, subcommand: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"));
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("PATH", &self.bin)
            .env("HERDR_PLUGIN_CONFIG_DIR", &self.config)
            .env("HERDR_PLUGIN_STATE_DIR", self._dir.path().join("state"))
            .env("SHEPHERD_TEST_ALLOW_LOCAL_ORIGIN", "allow-local-in-tmp")
            .env("SHEPHERD_TEST_ARGS", &self.args)
            .env("SHEPHERD_TEST_INPUT", &self.input)
            .env(
                "SHEPHERD_TEST_FIXTURE",
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/gh_pr_view.json"
                ),
            )
            .env_remove("SHEPHERD_TEST_ERROR")
            .args(["--json", "pr", subcommand, "--worktree"])
            .arg(&self.repo);
        command
    }
}

#[test]
fn view_auth_and_rate_limit_are_github_unavailable() {
    let fixture = FakeGh::new();
    for message in [
        "To get started with GitHub CLI, please run: gh auth login",
        "HTTP 403: API rate limit exceeded",
    ] {
        let out = fixture
            .command("view")
            .env("SHEPHERD_TEST_ERROR", message)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(value["error"]["code"], "github_unavailable");
    }
}

#[test]
fn view_no_pr_is_success() {
    let fixture = FakeGh::new();
    let out = fixture
        .command("view")
        .env(
            "SHEPHERD_TEST_ERROR",
            "no pull requests found for branch feat/test",
        )
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(value["data"].is_null());
}

#[test]
fn backend_create_uses_stdin_without_draft() {
    let fixture = FakeGh::new();
    let out = fixture
        .command("create")
        .args([
            "--base",
            "main",
            "--title",
            "An ordinary title",
            "--body-file",
        ])
        .arg(&fixture.body)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        fs::read(&fixture.input).unwrap(),
        fs::read(&fixture.body).unwrap()
    );
    let args = fs::read_to_string(&fixture.args).unwrap();
    assert!(args.contains("pr\ncreate\n--base\nmain\n--title\nAn ordinary title\n--body-file\n-\n--head\nfeat/test\n"),"{args}");
    assert!(!args.contains("--draft"));
}

#[test]
fn backend_reply_uses_general_and_thread_forms() {
    let fixture = FakeGh::new();
    let out = fixture
        .command("reply")
        .args(["--number", "42", "--body-file"])
        .arg(&fixture.body)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        fs::read(&fixture.input).unwrap(),
        fs::read(&fixture.body).unwrap()
    );
    let out = fixture
        .command("reply")
        .args(["--number", "42", "--comment", "123", "--body-file"])
        .arg(&fixture.body)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let args = fs::read_to_string(&fixture.args).unwrap();
    assert!(args.contains("pr\ncomment\n42\n--body-file\n-\n"), "{args}");
    assert!(args.contains("api\n-X\nPOST\nrepos/example/repo/pulls/42/comments/123/replies\n-f\nbody=A literal reply: $(unchanged) 'quoted'\nsecond line\n"),"{args}");
}

#[test]
fn pr_push_requires_ticket_even_with_explicit_branch() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir(&root).unwrap();
    repo(&root, "trunk");
    let origin = dir.path().join("origin.git");
    git(&root, &["init", "--bare", origin.to_str().unwrap()]);
    git(
        &root,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&root, &["push", "origin", "trunk"]);
    let before = git(&origin, &["rev-parse", "trunk"]).stdout;
    git(
        &root,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "change",
        ],
    );
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(
        bin.join("gh"),
        "#!/bin/sh\necho 'no pull requests found' >&2\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(bin.join("gh"), fs::Permissions::from_mode(0o700)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "PATH",
            std::env::join_paths([bin, "/usr/bin".into(), "/bin".into()]).unwrap(),
        )
        .args(["--json", "--state-dir"])
        .arg(dir.path().join("state"))
        .args(["--config-dir"])
        .arg(dir.path().join("cfg"))
        .args(["pr", "push", "--worktree"])
        .arg(&root)
        .args(["--branch", "trunk"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(before, git(&origin, &["rev-parse", "trunk"]).stdout);
}
