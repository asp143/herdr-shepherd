use herdr_shepherd::gate::shim::install_shim;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::Mutex,
};

static SCRIPT_EXEC_LOCK: Mutex<()> = Mutex::new(());

struct Fixture {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    remote: PathBuf,
    shim: PathBuf,
    path: std::ffi::OsString,
    marker: PathBuf,
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let remote = dir.path().join("origin.git");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["init", "--bare", remote.to_str().unwrap()]);
        git(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        fs::write(repo.join("fixture"), "fixture\n").unwrap();
        git(&repo, &["add", "fixture"]);
        git(&repo, &["commit", "-m", "fixture"]);
        git(
            &repo,
            &["push", "origin", "refs/heads/main:refs/heads/main"],
        );
        let fake = dir.path().join("fake");
        fs::create_dir(&fake).unwrap();
        let marker = dir.path().join("executed");
        fs::write(
            fake.join("gh"),
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"$GH_FIXTURE_MARKER\"\nprintf 'FIXTURE_GH\\n'\n",
        )
        .unwrap();
        fs::set_permissions(fake.join("gh"), fs::Permissions::from_mode(0o700)).unwrap();
        let shim = install_shim(
            &dir.path().join("state"),
            Path::new(env!("CARGO_BIN_EXE_herdr-shepherd")),
        )
        .unwrap();
        let store = herdr_shepherd::store::Store::open(&dir.path().join("state"), &repo).unwrap();
        store
            .update(|data| {
                data.trust = Some(herdr_shepherd::store::ProjectTrust {
                    project_root: repo.clone(),
                    common_git_dir: repo.join(".git"),
                    origin_url: "https://github.com/o/r.git".into(),
                    push_url: remote.to_string_lossy().into_owned(),
                    default_branch: "main".into(),
                    config_commit: None,
                    config_blob: None,
                    config_text: None,
                    trusted_at: "fixture".into(),
                    global_config: None,
                    object_format: "sha1".into(),
                });
                Ok(())
            })
            .unwrap();
        let path = std::env::join_paths([shim.clone(), fake]).unwrap();
        Self {
            _dir: dir,
            repo,
            remote,
            shim,
            path,
            marker,
        }
    }
    fn invoke(&self, args: &[&str]) -> Output {
        Command::new(self.shim.join("gh"))
            .current_dir(&self.repo)
            .env("PATH", &self.path)
            .env("GH_FIXTURE_MARKER", &self.marker)
            .args(args)
            .output()
            .unwrap()
    }
    fn assert_blocked(&self, args: &[&str]) {
        let before = git(&self.remote, &["show-ref"]);
        let out = self.invoke(args);
        assert_eq!(
            out.status.code(),
            Some(4),
            "{args:?}: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !self.marker.exists(),
            "blocked call executed fake gh: {args:?}"
        );
        assert_eq!(git(&self.remote, &["show-ref"]), before);
    }
}

#[test]
fn shim_blocks_repository_ref_api_writes() {
    let _guard = SCRIPT_EXEC_LOCK.lock().unwrap();
    let f = Fixture::new();
    f.assert_blocked(&[
        "api",
        "repos/o/r/git/refs",
        "-f",
        "ref=refs/heads/main",
        "-f",
        "sha=fixture",
    ]);
}

#[test]
fn shim_blocks_contents_branch_and_workflow_api_writes() {
    let _guard = SCRIPT_EXEC_LOCK.lock().unwrap();
    let f = Fixture::new();
    for args in [
        vec![
            "api",
            "repos/o/r/contents/file",
            "-X",
            "PUT",
            "-f",
            "content=fixture",
        ],
        vec!["api", "repos/o/r/branches/main/protection", "-XDELETE"],
        vec![
            "api",
            "repos/o/r/branches/main/rename",
            "--field=new_name=other",
        ],
        vec!["api", "repos/o/r/merge-upstream", "--input=-"],
        vec![
            "api",
            "repos/o/r/actions/workflows/deploy.yml/dispatches",
            "-X",
            "POST",
        ],
        vec![
            "api",
            "https://api.github.com/REPOS/o/r/CONTENTS/file?ref=main",
            "--method=GET",
            "-F",
            "content=fixture",
        ],
        vec!["api", "repos/o/r/git/refs/heads/main", "-XPATCH"],
        vec!["api", "repos/o/r/git/%72efs", "-f", "ref=main"],
        vec![
            "api",
            "repos/o/r/branches/main/%70rotection",
            "-X",
            "DELETE",
        ],
    ] {
        f.assert_blocked(&args);
    }
}

#[test]
fn shim_blocks_repository_and_workflow_cli_mutations() {
    let _guard = SCRIPT_EXEC_LOCK.lock().unwrap();
    let f = Fixture::new();
    for args in [
        vec!["repo", "delete", "o/r", "--yes"],
        vec!["repo", "edit", "o/r", "--default-branch", "other"],
        vec!["repo", "rename", "other", "--repo", "o/r"],
        vec!["repo", "sync", "o/r", "--force"],
        vec!["repo", "fork", "o/r"],
        vec!["repo", "list", "o"],
        vec!["repo", "clone", "o/r"],
        vec!["workflow", "run", "deploy.yml", "--ref", "main"],
        vec!["workflow", "enable", "deploy.yml"],
        vec!["release", "create", "v1"],
        vec!["release", "delete", "v1", "--yes"],
        vec!["release", "edit", "v1", "--latest"],
        vec![
            "api",
            "graphql",
            "-f",
            "query=mutation { addPullRequestReview(input:{event:APPROVE}) { clientMutationId } }",
        ],
    ] {
        f.assert_blocked(&args);
    }
}

#[test]
fn shim_preserves_read_only_repo_commands_and_comment_replies() {
    let _guard = SCRIPT_EXEC_LOCK.lock().unwrap();
    let f = Fixture::new();
    for args in [
        vec!["repo", "view", "--json", "defaultBranchRef"],
        vec!["workflow", "list"],
        vec!["workflow", "view", "deploy.yml"],
        vec!["api", "repos/o/r/contents/file"],
        vec!["api", "repos/o/r/branches/main/protection", "-XGET"],
        vec![
            "api",
            "-X",
            "POST",
            "repos/o/r/pulls/1/comments/2/replies",
            "-f",
            "body=Document git/refs and merges routes",
        ],
        vec![
            "pr",
            "comment",
            "1",
            "--body",
            "Do not workflow run or repo delete",
        ],
    ] {
        let out = f.invoke(&args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), "FIXTURE_GH\n");
        let mut expected = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        if args.starts_with(&["repo", "view"]) {
            expected.insert(2, "o/r".into());
        } else if args[0] != "api" {
            expected.extend(["-R".into(), "o/r".into()]);
        }
        assert_eq!(
            fs::read_to_string(&f.marker).unwrap().trim(),
            expected.join(" ")
        );
    }
}

#[test]
fn shim_canonical_path_aliases_do_not_recurse() {
    let _guard = SCRIPT_EXEC_LOCK.lock().unwrap();
    let mut f = Fixture::new();
    let alias = f._dir.path().join("shim-alias");
    std::os::unix::fs::symlink(&f.shim, &alias).unwrap();
    let fake = f._dir.path().join("fake");
    f.path = std::env::join_paths([f.shim.clone(), alias, f.shim.join("..").join("shim"), fake])
        .unwrap();
    let out = f.invoke(&["pr", "view", "1"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "FIXTURE_GH\n");
}

#[test]
fn shim_sensitive_api_options_cannot_hide_writes() {
    let _guard = SCRIPT_EXEC_LOCK.lock().unwrap();
    let f = Fixture::new();
    for options in [
        vec!["-fref=refs/heads/main"],
        vec!["--raw-field=ref=refs/heads/main"],
        vec!["-F", "ref=refs/heads/main"],
        vec!["--field", "ref=refs/heads/main"],
        vec!["--input=fixture.json"],
        vec!["--method=POST"],
        vec!["-X=PATCH"],
        vec!["-XDELETE"],
        vec!["-X", "HEAD"],
        vec!["-XGET", "-f", "ref=refs/heads/main"],
    ] {
        let args = [vec!["api", "repos/o/r/git/refs/heads/main"], options].concat();
        f.assert_blocked(&args);
    }
}

#[test]
fn shim_blocks_repository_settings_and_ruleset_writes() {
    let _guard = SCRIPT_EXEC_LOCK.lock().unwrap();
    let f = Fixture::new();
    for args in [
        vec!["api", "repos/o/r", "-XPATCH", "-fdefault_branch=other"],
        vec!["api", "repos/o/r", "-XDELETE"],
        vec![
            "api",
            "repos/o/r/rulesets/1",
            "-XPATCH",
            "-fenforcement=disabled",
        ],
    ] {
        f.assert_blocked(&args);
    }
}

#[test]
fn shim_blocks_issue_and_run_mutations() {
    let _guard = SCRIPT_EXEC_LOCK.lock().unwrap();
    let f = Fixture::new();
    for args in [
        vec!["issue", "delete", "7", "--yes"],
        vec!["issue", "edit", "7", "--title", "changed"],
        vec!["run", "cancel", "7"],
        vec!["run", "rerun", "7"],
        vec!["run", "delete", "7"],
    ] {
        f.assert_blocked(&args);
    }
    for args in [
        vec!["issue", "view", "7"],
        vec!["issue", "list"],
        vec!["issue", "status"],
        vec!["run", "view", "7"],
        vec!["run", "list"],
        vec!["run", "watch", "7"],
    ] {
        if f.marker.exists() {
            fs::remove_file(&f.marker).unwrap();
        }
        let out = f.invoke(&args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(f.marker.exists());
    }
}
