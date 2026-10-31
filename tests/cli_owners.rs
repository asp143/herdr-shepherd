use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

fn git(path: &Path, args: &[&str]) {
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
}

#[test]
fn create_requests_only_missing_codeowners() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "--quiet", "--initial-branch", "feat/test"]);
    git(&repo, &["config", "user.name", "Fixture"]);
    git(&repo, &["config", "user.email", "fixture@example.invalid"]);
    git(&repo, &["commit", "--quiet", "--allow-empty", "-m", "base"]);

    fs::create_dir(repo.join(".github")).unwrap();
    fs::write(
        repo.join(".github/CODEOWNERS"),
        "* @fixture-author @already @new @org/team @org/other\n",
    )
    .unwrap();
    git(&repo, &["add", ".github/CODEOWNERS"]);
    git(&repo, &["commit", "--quiet", "-m", "owners"]);
    git(&repo, &["branch", "main"]);
    fs::write(repo.join("ordinary.txt"), "changed\n").unwrap();
    git(&repo, &["add", "ordinary.txt"]);
    git(&repo, &["commit", "--quiet", "-m", "change"]);
    let state = dir.path().join("state");
    let remote = dir.path().join("remote.git");
    git(
        &repo,
        &[
            "clone",
            "--bare",
            "--quiet",
            repo.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    let store = herdr_shepherd::store::Store::open(&state, &repo).unwrap();
    store
        .update(|data| {
            data.trust = Some(herdr_shepherd::store::ProjectTrust {
                project_root: repo.clone(),
                common_git_dir: repo.join(".git"),
                origin_url: "https://github.com/example/project.git".into(),
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
                worktree: Some(repo.clone()),
                ..Default::default()
            });
            Ok(())
        })
        .unwrap();
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
    fs::write(bin.join("gh"),r#"#!/bin/sh
printf 'CALL\n' >> "$SHEPHERD_TEST_ARGS"
printf '%s\n' "$@" >> "$SHEPHERD_TEST_ARGS"
case "$1 $2" in
  'api user') printf '%s\n' fixture-author ;;
  'pr create') /bin/cat >/dev/null; printf '%s\n' 'https://github.com/example/project/pull/42' ;;
  'pr view')
    if [ "$5" = reviewRequests ]; then
      printf '%s\n' '{"reviewRequests":[{"login":"already"},{"__typename":"Team","name":"Team","slug":"org/team"}]}'
    else
      /bin/cat "$SHEPHERD_TEST_FIXTURE"
    fi ;;
  'pr edit') printf '%s\n' '{}' ;;
  *) printf '%s\n' unexpected-fake-command >&2; exit 9 ;;
esac
"#).unwrap();
    fs::set_permissions(bin.join("gh"), fs::Permissions::from_mode(0o700)).unwrap();
    let args = dir.path().join("args.txt");
    let body = dir.path().join("body.txt");
    fs::write(&body, "ordinary body\n").unwrap();
    let command = |sub: &str| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"));
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("PATH", &bin)
            .env("HERDR_PLUGIN_CONFIG_DIR", dir.path().join("config"))
            .env("HERDR_PLUGIN_STATE_DIR", &state)
            .env("SHEPHERD_TEST_ALLOW_LOCAL_ORIGIN", "allow-local-in-tmp")
            .env("SHEPHERD_TEST_ARGS", &args)
            .env(
                "SHEPHERD_TEST_FIXTURE",
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/gh_pr_view.json"
                ),
            )
            .args(["--json", "pr", sub, "--worktree"])
            .arg(&repo);
        cmd
    };
    let owners = command("owners").args(["--base", "main"]).output().unwrap();
    assert!(
        owners.status.success(),
        "{}",
        String::from_utf8_lossy(&owners.stdout)
    );
    let value: serde_json::Value = serde_json::from_slice(&owners.stdout).unwrap();
    assert_eq!(
        value["data"],
        serde_json::json!(["already", "new", "org/other", "org/team"])
    );
    let out = command("create")
        .args(["--base", "main", "--title", "Fixture", "--body-file"])
        .arg(&body)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let args = fs::read_to_string(args).unwrap();
    assert!(
        args.contains("pr\nedit\n42\n--add-reviewer\nnew\n--add-reviewer\norg/other\n"),
        "{args}"
    );
    for forbidden in [
        "--add-reviewer\nalready\n",
        "--add-reviewer\nfixture-author\n",
        "--add-reviewer\norg/team\n",
        "--add-label",
        "--add-assignee",
    ] {
        assert!(!args.contains(forbidden), "{args}");
    }
}
