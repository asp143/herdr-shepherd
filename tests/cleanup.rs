use herdr_shepherd::{
    cleanup::{self, RemoteBranch},
    config::{self, Config},
    error::ShepherdError,
    gate::Gate,
    github::{Checks, PrBackend, PrState, PrStatus, ReviewDecision},
    herdr::{AgentInfo, Herdr, Workspace, WorktreeInfo},
};
use std::{
    cell::RefCell,
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
    let out = git(path, args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
struct Backend(PrStatus);
impl PrBackend for Backend {
    fn view(&self, _: &Path, _: &str) -> Result<Option<PrStatus>, ShepherdError> {
        Ok(Some(self.0.clone()))
    }
    fn push(&self, _: &Gate) -> Result<(), ShepherdError> {
        panic!("unexpected backend push")
    }
    fn create(&self, _: &Gate, _: &str, _: &str, _: &str) -> Result<PrStatus, ShepherdError> {
        panic!("unexpected create")
    }
    fn add_reviewers(&self, _: &Gate, _: u64, _: &[String]) -> Result<(), ShepherdError> {
        panic!("unexpected reviewers")
    }
    fn reply(&self, _: &Gate, _: u64, _: Option<u64>, _: &str) -> Result<(), ShepherdError> {
        panic!("unexpected reply")
    }
}
struct LocalHerdr {
    main: PathBuf,
    workspaces: RefCell<Vec<Workspace>>,
    removals: RefCell<Vec<String>>,
    agents: RefCell<Vec<AgentInfo>>,
    notifications: RefCell<Vec<String>>,
    rewrite_after_remove: RefCell<Option<(String, String)>>,
    require_object_before_remove: RefCell<Option<String>>,
}
impl Herdr for LocalHerdr {
    fn workspaces(&self) -> Result<Vec<Workspace>, ShepherdError> {
        Ok(self.workspaces.borrow().clone())
    }
    fn agents(&self) -> Result<Vec<AgentInfo>, ShepherdError> {
        Ok(self.agents.borrow().clone())
    }
    fn rename_workspace(&self, _: &str, _: &str) -> Result<(), ShepherdError> {
        Ok(())
    }
    fn move_block(&self, _: &[String], _: Option<&str>) -> Result<(), ShepherdError> {
        Ok(())
    }
    fn report_workspace_tokens(
        &self,
        _: &str,
        _: &[(String, String)],
        _: &[String],
    ) -> Result<(), ShepherdError> {
        Ok(())
    }
    fn report_pane_tokens(
        &self,
        _: &str,
        _: &[(String, String)],
        _: &[String],
    ) -> Result<(), ShepherdError> {
        Ok(())
    }
    fn notify(&self, title: &str, body: &str) -> Result<(), ShepherdError> {
        self.notifications
            .borrow_mut()
            .push(format!("{title}: {body}"));
        Ok(())
    }
    fn remove_worktree(&self, ws: &str, force: bool) -> Result<(), ShepherdError> {
        if let Some(sha) = self.require_object_before_remove.borrow().as_ref() {
            assert!(
                git(&self.main, &["cat-file", "-e", sha]).status.success(),
                "verified remote objects were missing before removal"
            );
        }
        let path = self
            .workspaces
            .borrow()
            .iter()
            .find(|w| w.workspace_id == ws)
            .unwrap()
            .worktree
            .as_ref()
            .unwrap()
            .checkout_path
            .clone()
            .unwrap();
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(path.to_str().unwrap());
        ok(&self.main, &args);
        self.removals.borrow_mut().push(ws.into());
        if let Some((reference, tip)) = self.rewrite_after_remove.borrow_mut().take() {
            ok(&self.main, &["update-ref", &reference, &tip]);
        }
        self.workspaces
            .borrow_mut()
            .retain(|w| w.workspace_id != ws);
        Ok(())
    }
    fn close_workspace(&self, ws: &str) -> Result<(), ShepherdError> {
        self.workspaces
            .borrow_mut()
            .retain(|w| w.workspace_id != ws);
        Ok(())
    }
}
struct Repo {
    _dir: tempfile::TempDir,
    main: PathBuf,
    ticket: PathBuf,
    h: LocalHerdr,
    backend: Backend,
    cfg: Config,
    state: PathBuf,
}
impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        let remote = dir.path().join("remote.git");
        let ticket = dir.path().join("ticket");
        fs::create_dir(&main).unwrap();
        ok(&main, &["init", "-b", "main"]);
        // Library calls launch git in this process, so pin harmless local
        // settings as well as isolating the fixture setup commands above.
        let hooks = dir.path().join("empty-hooks");
        fs::create_dir(&hooks).unwrap();
        ok(
            &main,
            &["config", "core.hooksPath", hooks.to_str().unwrap()],
        );
        ok(&main, &["config", "commit.gpgsign", "false"]);
        ok(&main, &["config", "tag.gpgsign", "false"]);

        ok(&main, &["config", "user.email", "fixture@example.invalid"]);
        ok(&main, &["config", "user.name", "Fixture"]);
        fs::write(main.join("base"), "base\n").unwrap();
        ok(&main, &["add", "base"]);
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
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        ok(
            &main,
            &["worktree", "add", "-b", "feat/t", ticket.to_str().unwrap()],
        );
        ok(&main, &["config", "extensions.worktreeConfig", "true"]);
        ok(
            &ticket,
            &["config", "--worktree", "shepherd.ticketBranch", "feat/t"],
        );
        fs::write(ticket.join("ticket"), "ticket\n").unwrap();
        ok(&ticket, &["add", "ticket"]);
        ok(&ticket, &["commit", "-m", "ticket"]);
        ok(&ticket, &["push", "-u", "origin", "feat/t"]);
        let ws = |id: &str, path: &Path, linked| Workspace {
            workspace_id: id.into(),
            label: id.into(),
            worktree: Some(WorktreeInfo {
                repo_key: "fixture".into(),
                repo_root: Some(main.clone()),
                checkout_path: Some(path.into()),
                is_linked_worktree: linked,
            }),
        };
        let h = LocalHerdr {
            main: main.clone(),
            workspaces: RefCell::new(vec![ws("main", &main, false), ws("ticket", &ticket, true)]),
            removals: RefCell::new(vec![]),
            agents: RefCell::new(vec![]),
            notifications: RefCell::new(vec![]),
            rewrite_after_remove: RefCell::new(None),
            require_object_before_remove: RefCell::new(None),
        };
        let backend = Backend(PrStatus {
            number: 1,
            url: "https://example.invalid/fixture/pull/1".into(),
            state: PrState::Merged,
            merged_at: Some("2026-10-31T00:00:00Z".into()),
            base: "main".into(),
            head: "feat/t".into(),
            checks: Checks::Passing,
            review: ReviewDecision::Approved,
            comment_ids: vec![],
            head_oid: Some(ok(&ticket, &["rev-parse", "HEAD"]).trim().into()),
        });
        let cfg = config::load(&dir.path().join("config"), None).unwrap();
        let state = dir.path().join("state");
        let store = herdr_shepherd::store::Store::open(&state, &main).unwrap();
        store
            .update(|data| {
                data.tickets.push(herdr_shepherd::store::Ticket {
                    id: "T".into(),
                    branch: "feat/t".into(),
                    base: "main".into(),
                    worktree: Some(ticket.clone()),
                    workspace_id: Some("ticket".into()),
                    ..Default::default()
                });
                Ok(())
            })
            .unwrap();
        let file = store.root().join("state.json");
        let mut data: serde_json::Value =
            serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        data["trust"] = serde_json::json!({"project_root":main,"common_git_dir":main.join(".git"),"origin_url":remote.to_str().unwrap(),"push_url":remote.to_str().unwrap(),"default_branch":"main","config_commit":null,"config_blob":null,"config_text":null,"trusted_at":"fixture","global_config":"/dev/null","object_format":"sha1"});
        fs::write(file, serde_json::to_vec(&data).unwrap()).unwrap();
        Self {
            _dir: dir,
            main,
            ticket,
            h,
            backend,
            cfg,
            state,
        }
    }
    fn transport(&self) -> herdr_shepherd::gate::transport::PushTransport {
        herdr_shepherd::gate::transport::PushTransport::open_with_options(
            &self.state,
            &self.ticket,
            herdr_shepherd::gate::transport::TransportOptions {
                allow_local_for_tests: true,
            },
        )
        .unwrap()
    }
    fn preflight(&self, dry_run: bool) -> Result<cleanup::Preflight<'_>, ShepherdError> {
        cleanup::preflight_with_transport(
            &self.h,
            &self.backend,
            &self.cfg,
            &self.ticket,
            false,
            dry_run,
            self.transport(),
        )
    }
    fn merge(&self) {
        ok(&self.main, &["merge", "--ff-only", "feat/t"]);
        ok(&self.main, &["push", "origin", "main"]);
    }
}
#[test]
fn base_sync_never_executes_agent_smudge_filter() {
    use std::os::unix::fs::PermissionsExt;
    let mut repo = Repo::new();
    let before = ok(&repo.main, &["rev-parse", "HEAD"]).trim().to_owned();
    fs::write(repo.ticket.join(".gitattributes"), "*.txt filter=agent\n").unwrap();
    fs::write(repo.ticket.join("synced.txt"), "merged content\n").unwrap();
    ok(&repo.ticket, &["add", ".gitattributes", "synced.txt"]);
    ok(&repo.ticket, &["commit", "-m", "add attributed file"]);
    ok(&repo.ticket, &["push", "origin", "feat/t"]);
    let target = ok(&repo.ticket, &["rev-parse", "HEAD"]).trim().to_owned();
    repo.backend.0.head_oid = Some(target.clone());
    repo.merge();
    ok(&repo.main, &["reset", "--hard", &before]);
    let marker = repo._dir.path().join("agent-smudge-executed");
    let helper = repo._dir.path().join("agent-smudge");
    fs::write(
        &helper,
        format!("#!/bin/sh\nprintf ran > '{}'\ncat\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
    ok(
        &repo.main,
        &["config", "filter.agent.smudge", helper.to_str().unwrap()],
    );
    let report = cleanup::run(repo.preflight(false).unwrap(), false).unwrap();
    assert!(report.base_synced.is_ok(), "{:?}", report.base_synced);
    assert!(
        !marker.exists(),
        "base synchronization executed agent Git config"
    );
    assert_eq!(ok(&repo.main, &["rev-parse", "HEAD"]).trim(), target);
    assert_eq!(
        fs::read_to_string(repo.main.join("synced.txt")).unwrap(),
        "merged content\n"
    );
}

#[test]
fn local_branch_deletion_never_runs_agent_ref_hook() {
    use std::os::unix::fs::PermissionsExt;
    let repo = Repo::new();
    repo.merge();
    let preflight = repo.preflight(false).unwrap();
    let hooks = repo._dir.path().join("agent-hooks");
    fs::create_dir(&hooks).unwrap();
    let marker = repo._dir.path().join("agent-ref-hook-executed");
    let hook = hooks.join("reference-transaction");
    fs::write(
        &hook,
        format!("#!/bin/sh\nprintf ran > '{}'\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    ok(
        &repo.main,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
    let report = cleanup::run(preflight, false).unwrap();
    assert!(report.local_branch_deleted);
    assert!(
        !marker.exists(),
        "cleanup executed agent reference-transaction hook"
    );
}

#[test]
fn refuses_dirty_without_force_discard() {
    let repo = Repo::new();
    fs::write(repo.ticket.join("dirty"), "dirty").unwrap();
    let err = repo.preflight(false).err().expect("dirty rejected");
    assert!(err.to_string().contains("dirty"));
    assert!(repo.h.removals.borrow().is_empty());
}
#[test]
fn refuses_open_pr() {
    let mut repo = Repo::new();
    repo.backend.0.state = PrState::Open;
    repo.backend.0.merged_at = None;
    let err = repo.preflight(false).err().expect("open rejected");
    assert!(err.to_string().contains("not merged"));
    assert!(repo.h.removals.borrow().is_empty());
}
#[test]
fn squash_merged_gone_remote_dirty_main() {
    let repo = Repo::new();
    ok(&repo.main, &["merge", "--squash", "feat/t"]);
    ok(&repo.main, &["commit", "-m", "squash"]);
    ok(&repo.main, &["push", "origin", "main"]);
    ok(&repo.main, &["push", "origin", "--delete", "feat/t"]);
    fs::write(repo.main.join("dirty"), "dirty").unwrap();
    let report = cleanup::run(repo.preflight(false).unwrap(), false).unwrap();
    assert!(!report.local_branch_deleted);
    assert!(report.local_branch_kept_reason.is_some());
    assert_eq!(report.remote_branch, RemoteBranch::AlreadyGone);
    assert_eq!(report.base_synced, Err("main_checkout_dirty".into()));
    assert!(
        git(&repo.main, &["show-ref", "--verify", "refs/heads/feat/t"])
            .status
            .success()
    );
}
#[test]
fn ancestor_branch_deleted() {
    let repo = Repo::new();
    repo.merge();
    let report = cleanup::run(repo.preflight(false).unwrap(), false).unwrap();
    assert!(report.workspace_removed);
    assert!(report.local_branch_deleted);
    assert_eq!(report.remote_branch, RemoteBranch::Deleted);
    assert!(report.base_synced.is_ok());
    assert!(!repo.ticket.exists());
    assert!(
        !git(&repo.main, &["show-ref", "--verify", "refs/heads/feat/t"])
            .status
            .success()
    );
}
#[test]
fn dry_run_changes_nothing() {
    let repo = Repo::new();
    repo.merge();
    let before = (
        ok(&repo.main, &["for-each-ref"]),
        ok(&repo.main, &["worktree", "list", "--porcelain"]),
    );
    let report = cleanup::run(repo.preflight(true).unwrap(), true).unwrap();
    assert!(report.dry_run);
    let after = (
        ok(&repo.main, &["for-each-ref"]),
        ok(&repo.main, &["worktree", "list", "--porcelain"]),
    );
    assert_eq!(before, after);
    assert!(repo.h.removals.borrow().is_empty());
    assert!(repo.ticket.exists());
}
#[test]
fn included_config_cannot_forge_ownership_marker() {
    let repo = Repo::new();
    let included = repo._dir.path().join("agent-marker.gitconfig");
    fs::write(&included, "[shepherd]\nticketBranch = feat/t\n").unwrap();
    ok(
        &repo.ticket,
        &["config", "--worktree", "--unset", "shepherd.ticketBranch"],
    );
    ok(
        &repo.ticket,
        &[
            "config",
            "--worktree",
            "include.path",
            included.to_str().unwrap(),
        ],
    );
    let error = repo
        .preflight(false)
        .err()
        .expect("included marker must not prove ownership");
    assert!(error.to_string().contains("owned"));
    assert!(repo.h.removals.borrow().is_empty());
}

#[test]
fn marker_missing_refused() {
    let repo = Repo::new();
    ok(
        &repo.ticket,
        &["config", "--worktree", "--unset", "shepherd.ticketBranch"],
    );
    let err = repo.preflight(false).err().expect("unowned rejected");
    assert!(err.to_string().contains("owned"));
}

fn child(repo: &Repo, owned: bool) -> PathBuf {
    let path = repo._dir.path().join("child");
    ok(
        &repo.main,
        &[
            "worktree",
            "add",
            "-b",
            "feat/t--1",
            path.to_str().unwrap(),
            "feat/t",
        ],
    );
    if owned {
        ok(
            &path,
            &["config", "--worktree", "shepherd.ticketBranch", "feat/t--1"],
        );
    }
    repo.h.workspaces.borrow_mut().push(Workspace {
        workspace_id: "child".into(),
        label: "child".into(),
        worktree: Some(WorktreeInfo {
            repo_key: "fixture".into(),
            repo_root: Some(repo.main.clone()),
            checkout_path: Some(path.clone()),
            is_linked_worktree: true,
        }),
    });
    path
}

#[test]
fn owned_child_removed_before_ticket() {
    let repo = Repo::new();
    let child = child(&repo, true);
    repo.merge();
    let report = cleanup::run(repo.preflight(false).unwrap(), false).unwrap();
    assert_eq!(
        report.children_removed.as_slice(),
        std::slice::from_ref(&child)
    );
    assert!(!child.exists());
    assert_eq!(*repo.h.removals.borrow(), ["child", "ticket"]);
}

#[test]
fn dirty_child_after_preflight_blocks_all_removal() {
    let repo = Repo::new();
    let child = child(&repo, true);
    repo.merge();
    let preflight = repo.preflight(false).unwrap();
    fs::write(child.join("dirty"), "dirty").unwrap();
    let error = cleanup::run(preflight, false).unwrap_err();
    assert!(error.to_string().contains("dirty"));
    assert!(repo.h.removals.borrow().is_empty());
    assert!(repo.ticket.exists());
}

#[test]
fn unowned_child_blocks_all_cleanup() {
    let repo = Repo::new();
    child(&repo, false);
    repo.merge();
    let error = repo.preflight(false).err().expect("unowned child rejected");
    assert!(error.to_string().contains("owned"));
    assert!(repo.h.removals.borrow().is_empty());
}

#[test]
fn force_discard_is_explicit() {
    let repo = Repo::new();
    repo.merge();
    fs::write(repo.ticket.join("dirty"), "discard this fixture").unwrap();
    let preflight = cleanup::preflight_with_transport(
        &repo.h,
        &repo.backend,
        &repo.cfg,
        &repo.ticket,
        true,
        false,
        repo.transport(),
    )
    .unwrap();
    let report = cleanup::run(preflight, false).unwrap();
    assert!(report.workspace_removed);
    assert!(!repo.ticket.exists());
}

#[test]
fn main_checkout_not_on_base_skips_sync() {
    let repo = Repo::new();
    repo.merge();
    ok(&repo.main, &["branch", "other", "main"]);
    ok(&repo.main, &["symbolic-ref", "HEAD", "refs/heads/other"]);
    let report = cleanup::run(repo.preflight(false).unwrap(), false).unwrap();
    assert_eq!(report.base_synced, Err("main_checkout_not_on_base".into()));
    assert_eq!(
        ok(&repo.main, &["branch", "--show-current"]).trim(),
        "other"
    );
}

#[test]
fn squash_remote_branch_is_retained() {
    let repo = Repo::new();
    ok(&repo.main, &["merge", "--squash", "feat/t"]);
    ok(&repo.main, &["commit", "-m", "squash"]);
    ok(&repo.main, &["push", "origin", "main"]);
    let report = cleanup::run(repo.preflight(false).unwrap(), false).unwrap();
    assert!(!report.local_branch_deleted);
    assert_eq!(
        report.remote_branch,
        RemoteBranch::Skipped("branch_not_contained".into())
    );
    assert!(
        !ok(
            &repo.main,
            &["ls-remote", "--heads", "origin", "refs/heads/feat/t"]
        )
        .is_empty()
    );
}

#[test]
fn dry_run_failure_does_not_notify() {
    let repo = Repo::new();
    fs::write(repo.ticket.join("dirty"), "dirty").unwrap();
    assert!(repo.preflight(true).is_err());
    assert!(repo.h.notifications.borrow().is_empty());
    assert!(repo.h.removals.borrow().is_empty());
}

#[test]
fn symlinked_agent_cwd_blocks_cleanup() {
    let repo = Repo::new();
    fs::create_dir(repo.ticket.join("nested")).unwrap();
    let link = repo._dir.path().join("agent-cwd");
    std::os::unix::fs::symlink(repo.ticket.join("nested"), &link).unwrap();
    repo.h.agents.borrow_mut().push(AgentInfo {
        name: Some("fixture-agent".into()),
        agent: "fixture".into(),
        agent_status: "working".into(),
        cwd: Some(link),
        pane_id: "pane".into(),
        workspace_id: None,
    });
    let error = repo
        .preflight(false)
        .err()
        .expect("agent should block cleanup");
    assert!(error.to_string().contains("agents"));
    assert!(repo.h.removals.borrow().is_empty());
}

#[test]
fn preflight_does_not_launch_fsmonitor() {
    use std::os::unix::fs::PermissionsExt;
    let repo = Repo::new();
    let script = repo._dir.path().join("fsmonitor");
    fs::write(&script, "#!/bin/sh\n: > \"$0.called\"\nprintf '\\0'\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    ok(
        &repo.main,
        &["config", "core.fsmonitor", script.to_str().unwrap()],
    );
    repo.preflight(true).unwrap();
    assert!(!repo._dir.path().join("fsmonitor.called").exists());
}

#[test]
fn workspace_reassociation_stops_before_removal() {
    let repo = Repo::new();
    repo.merge();
    let preflight = repo.preflight(false).unwrap();
    let unrelated = repo._dir.path().join("unrelated");
    ok(
        &repo.main,
        &[
            "worktree",
            "add",
            "-b",
            "feat/unrelated",
            unrelated.to_str().unwrap(),
            "main",
        ],
    );
    let original = repo
        .h
        .workspaces
        .borrow()
        .iter()
        .find(|ws| ws.workspace_id == "ticket")
        .unwrap()
        .clone();
    let mut replacement = original.clone();
    replacement.workspace_id = "replacement".into();
    let mut reused = original;
    reused.worktree.as_mut().unwrap().checkout_path = Some(unrelated.clone());
    repo.h
        .workspaces
        .borrow_mut()
        .retain(|ws| ws.workspace_id != "ticket");
    repo.h.workspaces.borrow_mut().extend([replacement, reused]);
    let result = cleanup::run(preflight, false);
    assert!(result.is_err());
    assert!(
        repo.h.removals.borrow().is_empty(),
        "removed={:?}; unrelated_exists={}; target_exists={}",
        repo.h.removals.borrow(),
        unrelated.exists(),
        repo.ticket.exists()
    );
}

#[test]
fn new_commits_after_merge_block_cleanup() {
    let repo = Repo::new();
    repo.merge();
    fs::write(repo.ticket.join("post-merge"), "unmerged work\n").unwrap();
    ok(&repo.ticket, &["add", "post-merge"]);
    ok(
        &repo.ticket,
        &["commit", "-m", "new work after old PR merge"],
    );
    let result = repo.preflight(false);
    assert!(
        result.is_err(),
        "preflight accepted new commits with stale merged PR status"
    );
}

#[test]
fn shared_origin_change_cannot_redirect_cleanup() {
    let repo = Repo::new();
    repo.merge();
    let preflight = repo.preflight(false).unwrap();
    let other = repo._dir.path().join("other.git");
    ok(
        repo._dir.path(),
        &[
            "clone",
            "--bare",
            repo._dir.path().join("remote.git").to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    ok(
        &repo.main,
        &["remote", "set-url", "origin", other.to_str().unwrap()],
    );
    let result = cleanup::run(preflight, false).unwrap();
    assert!(result.workspace_removed);
    assert!(!repo.h.removals.borrow().is_empty());
    assert!(
        !git(
            &repo._dir.path().join("remote.git"),
            &["show-ref", "--verify", "refs/heads/feat/t"]
        )
        .status
        .success()
    );
    assert!(
        git(&other, &["show-ref", "--verify", "refs/heads/feat/t"])
            .status
            .success(),
        "untrusted replacement remote was changed"
    );
}

#[test]
fn remote_tip_advanced_during_receive_is_kept() {
    use std::os::unix::fs::PermissionsExt;
    let repo = Repo::new();
    repo.merge();
    let remote = repo._dir.path().join("remote.git");
    let old_tip = ok(&repo.ticket, &["rev-parse", "HEAD"]).trim().to_owned();
    let advanced = ok(
        &remote,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit-tree",
            &format!("{old_tip}^{{tree}}"),
            "-p",
            &old_tip,
            "-m",
            "remote-only work",
        ],
    )
    .trim()
    .to_owned();
    let hook = remote.join("hooks/pre-receive");
    fs::write(&hook,format!("#!/bin/sh\ngit --git-dir='{}' update-ref refs/heads/feat/t {advanced} {old_tip} || exit 8\n",remote.display())).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    let result = cleanup::run(repo.preflight(false).unwrap(), false);
    assert!(
        result.is_err(),
        "concurrently advanced remote deletion should refuse"
    );
    assert_eq!(
        ok(&remote, &["rev-parse", "refs/heads/feat/t"]).trim(),
        advanced
    );
}

#[test]
fn child_created_after_preflight_blocks_all_removals() {
    let repo = Repo::new();
    repo.merge();
    let preflight = repo.preflight(false).unwrap();
    let added = child(&repo, true);
    let result = cleanup::run(preflight, false);
    assert!(result.is_err());
    assert!(repo.h.removals.borrow().is_empty());
    assert!(added.exists());
    assert!(repo.ticket.exists());
}

#[test]
fn force_discard_does_not_remove_unowned_nested_worktree() {
    let repo = Repo::new();
    repo.merge();
    let nested = repo.ticket.join("nested");
    ok(
        &repo.main,
        &[
            "worktree",
            "add",
            "-b",
            "feat/unrelated",
            nested.to_str().unwrap(),
            "main",
        ],
    );
    let result = cleanup::preflight_with_transport(
        &repo.h,
        &repo.backend,
        &repo.cfg,
        &repo.ticket,
        true,
        false,
        repo.transport(),
    );
    assert!(result.is_err());
    assert!(repo.h.removals.borrow().is_empty());
    assert!(nested.exists());
}

#[test]
fn local_branch_changed_after_worktree_removal_is_kept() {
    let repo = Repo::new();
    repo.merge();
    let old = ok(&repo.ticket, &["rev-parse", "HEAD"]).trim().to_owned();
    let advanced = ok(
        &repo.main,
        &[
            "commit-tree",
            &format!("{old}^{{tree}}"),
            "-p",
            &old,
            "-m",
            "concurrent local work",
        ],
    )
    .trim()
    .to_owned();
    let preflight = repo.preflight(false).unwrap();
    *repo.h.rewrite_after_remove.borrow_mut() =
        Some(("refs/heads/feat/t".into(), advanced.clone()));
    let result = cleanup::run(preflight, false);
    assert!(result.is_err());
    assert_eq!(
        ok(&repo.main, &["rev-parse", "refs/heads/feat/t"]).trim(),
        advanced
    );
}

#[test]
fn uncontained_ticket_without_merged_head_proof_is_refused() {
    let mut repo = Repo::new();
    repo.backend.0.head_oid = None;
    assert!(repo.preflight(false).is_err());
    assert!(repo.h.removals.borrow().is_empty());
    assert!(repo.ticket.exists());
}

#[test]
fn cleanup_emits_lifecycle_after_workspace_removal() {
    let mut repo = Repo::new();
    repo.merge();
    let emitted = repo._dir.path().join("event.json");
    repo.cfg.raw.insert(
        "hooks".into(),
        toml::Value::Array(vec![toml::Value::Table(toml::Table::from_iter([
            ("on".into(), toml::Value::String("ticket.cleaned_up".into())),
            (
                "command".into(),
                toml::Value::Array(
                    [
                        "sh",
                        "-c",
                        "cat > \"$1\"",
                        "hook",
                        emitted.to_str().unwrap(),
                    ]
                    .into_iter()
                    .map(|s| toml::Value::String(s.into()))
                    .collect(),
                ),
            ),
        ]))]),
    );
    let report = cleanup::run(repo.preflight(false).unwrap(), false).unwrap();
    let event: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(emitted).unwrap()).unwrap();
    assert_eq!(
        event,
        serde_json::json!({"worktree":report.worktree,"branch":"feat/t","pr":1})
    );
    assert!(!repo.ticket.exists());
}

#[test]
fn dry_run_lists_ignored_files_before_removal() {
    let repo = Repo::new();
    let exclude = ok(&repo.ticket, &["rev-parse", "--git-path", "info/exclude"]);
    fs::write(exclude.trim(), " ignored-cache\n").unwrap();
    fs::write(repo.ticket.join(" ignored-cache"), "fixture cache\n").unwrap();
    let report = cleanup::run(repo.preflight(true).unwrap(), true).unwrap();
    assert!(
        report
            .ignored_files
            .contains(&repo.ticket.join(" ignored-cache"))
    );
    assert!(repo.ticket.join(" ignored-cache").exists());
}

#[test]
fn remote_only_merge_objects_exist_before_cleanup_mutates_worktrees() {
    let repo = Repo::new();
    let remote = repo._dir.path().join("remote.git");
    let other = repo._dir.path().join("other-clone");
    ok(
        repo._dir.path(),
        &[
            "clone",
            "--no-checkout",
            remote.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    ok(&other, &["config", "user.name", "Fixture"]);
    ok(&other, &["config", "user.email", "fixture@example.invalid"]);
    ok(&other, &["config", "commit.gpgsign", "false"]);
    ok(&other, &["symbolic-ref", "HEAD", "refs/heads/merge-work"]);
    ok(&other, &["reset", "--hard", "origin/main"]);
    ok(
        &other,
        &["merge", "--no-ff", "origin/feat/t", "-m", "remote merge"],
    );
    let remote_base = ok(&other, &["rev-parse", "HEAD"]).trim().to_owned();
    ok(&other, &["push", "origin", "HEAD:refs/heads/main"]);
    assert!(
        !git(&repo.main, &["cat-file", "-e", &remote_base])
            .status
            .success()
    );
    let global = repo._dir.path().join("transport-global.gitconfig");
    fs::write(&global, "[fetch]\nunpackLimit = 1\n").unwrap();
    herdr_shepherd::store::Store::open(&repo.state, &repo.main)
        .unwrap()
        .update(|data| {
            data.trust.as_mut().unwrap().global_config = Some(global);
            Ok(())
        })
        .unwrap();
    *repo.h.require_object_before_remove.borrow_mut() = Some(remote_base.clone());
    let preflight = repo.preflight(false).unwrap();
    let pack_dir = repo
        .state
        .join("push")
        .join(format!(
            "{}.git",
            herdr_shepherd::store::project_hash(&repo.main)
        ))
        .join("objects/pack");
    assert!(
        fs::read_dir(pack_dir).unwrap().any(|entry| entry
            .unwrap()
            .path()
            .extension()
            .is_some_and(|ext| ext == "pack")),
        "fixture must exercise packed-object import"
    );
    let report = cleanup::run(preflight, false).unwrap();
    assert!(report.workspace_removed);
    assert!(report.base_synced.is_ok(), "{:?}", report.base_synced);
    assert!(
        git(&repo.main, &["cat-file", "-e", &remote_base])
            .status
            .success()
    );
    assert_eq!(ok(&repo.main, &["rev-parse", "HEAD"]).trim(), remote_base);
}
