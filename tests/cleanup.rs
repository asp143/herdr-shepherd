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
    closed_tabs: RefCell<Vec<String>>,
    pane_sessions: RefCell<std::collections::HashMap<String, String>>,
    close_tab_effects: RefCell<bool>,
    fail_after_tab_close: RefCell<Option<usize>>,
    agent_after_close: RefCell<Option<AgentInfo>>,
    notifications: RefCell<Vec<String>>,
    rewrite_after_remove: RefCell<Option<(String, String)>>,
    require_object_before_remove: RefCell<Option<String>>,
    on_remove: RefCell<Option<Box<dyn FnOnce()>>>,
    fail_after_checkout_removal: RefCell<bool>,
}
impl Herdr for LocalHerdr {
    fn workspaces(&self) -> Result<Vec<Workspace>, ShepherdError> {
        Ok(self.workspaces.borrow().clone())
    }
    fn agents(&self) -> Result<Vec<AgentInfo>, ShepherdError> {
        Ok(self.agents.borrow().clone())
    }
    fn close_tab(&self, tab: &str) -> Result<(), ShepherdError> {
        self.closed_tabs.borrow_mut().push(tab.into());
        if *self.close_tab_effects.borrow() {
            let pane = tab.replace(":t", ":p");
            self.agents.borrow_mut().retain(|a| a.pane_id != pane);
            self.pane_sessions.borrow_mut().remove(&pane);
            if let Some(agent) = self.agent_after_close.borrow_mut().take() {
                self.agents.borrow_mut().push(agent);
            }
        }
        if *self.fail_after_tab_close.borrow() == Some(self.closed_tabs.borrow().len()) {
            self.fail_after_tab_close.replace(None);
            return Err(ShepherdError::Usage("lost close acknowledgement".into()));
        }
        Ok(())
    }
    fn pane_exists(&self, pane: &str) -> Result<bool, ShepherdError> {
        Ok(self.pane_sessions.borrow().contains_key(pane)
            || self.agents.borrow().iter().any(|a| a.pane_id == pane))
    }
    fn pane_session_ref(&self, pane: &str) -> Result<Option<String>, ShepherdError> {
        Ok(self.pane_sessions.borrow().get(pane).cloned())
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
        let output = git(&self.main, &args);
        if !output.status.success() {
            return Err(ShepherdError::Usage(
                String::from_utf8_lossy(&output.stderr).into(),
            ));
        }
        if self.fail_after_checkout_removal.replace(false) {
            return Err(ShepherdError::Usage(
                "injected failure after checkout removal".into(),
            ));
        }
        self.removals.borrow_mut().push(ws.into());
        if let Some((reference, tip)) = self.rewrite_after_remove.borrow_mut().take() {
            ok(&self.main, &["update-ref", &reference, &tip]);
        }
        self.workspaces
            .borrow_mut()
            .retain(|w| w.workspace_id != ws);
        if let Some(callback) = self.on_remove.borrow_mut().take() {
            callback();
        }
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
            closed_tabs: RefCell::default(),
            pane_sessions: RefCell::default(),
            close_tab_effects: RefCell::new(true),
            fail_after_tab_close: RefCell::new(None),
            agent_after_close: RefCell::new(None),
            notifications: RefCell::new(vec![]),
            rewrite_after_remove: RefCell::new(None),
            require_object_before_remove: RefCell::new(None),
            on_remove: RefCell::new(None),
            fail_after_checkout_removal: RefCell::new(false),
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
    herdr_shepherd::store::Store::open(&repo.state, &repo.main)
        .unwrap()
        .update(|data| {
            data.tickets[0]
                .children
                .push(herdr_shepherd::store::ChildWorktree {
                    unit: 1,
                    branch: "feat/t--1".into(),
                    path: path.clone(),
                    workspace_id: Some("child".into()),
                    base_sha: ok(&path, &["rev-parse", "HEAD"]).trim().into(),
                    integrated: true,
                });
            Ok(())
        })
        .unwrap();
    path
}

fn recorded_child(repo: &Repo, unit: u32, legacy: bool) -> PathBuf {
    let path = repo._dir.path().join(format!("child-{unit}"));
    let branch = format!("feat/t--{unit}");
    ok(
        &repo.main,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            path.to_str().unwrap(),
            "feat/t",
        ],
    );
    ok(
        &path,
        &["config", "--worktree", "shepherd.ticketBranch", &branch],
    );
    let workspace = legacy.then(|| format!("child-{unit}"));
    if let Some(id) = &workspace {
        repo.h.workspaces.borrow_mut().push(Workspace {
            workspace_id: id.clone(),
            label: id.clone(),
            worktree: Some(WorktreeInfo {
                repo_key: "fixture".into(),
                repo_root: Some(repo.main.clone()),
                checkout_path: Some(path.clone()),
                is_linked_worktree: true,
            }),
        });
    }
    let store = herdr_shepherd::store::Store::open(&repo.state, &repo.main).unwrap();
    store
        .update(|data| {
            data.tickets[0]
                .children
                .push(herdr_shepherd::store::ChildWorktree {
                    unit,
                    branch,
                    path: path.clone(),
                    workspace_id: workspace,
                    base_sha: ok(&path, &["rev-parse", "HEAD"]).trim().into(),
                    integrated: true,
                });
            Ok(())
        })
        .unwrap();
    path
}

#[test]
fn workspace_free_and_mixed_children_removed_before_ticket() {
    for legacy in [false, true] {
        let repo = Repo::new();
        let native = recorded_child(&repo, 1, false);
        let second = recorded_child(&repo, 2, legacy);
        repo.merge();
        let report = cleanup::run(repo.preflight(false).unwrap(), false).unwrap();
        assert_eq!(report.children_removed.len(), 2);
        assert!(!native.exists());
        assert!(!second.exists());
        assert!(!repo.ticket.exists());
        assert!(
            herdr_shepherd::store::Store::open(&repo.state, &repo.main)
                .unwrap()
                .read()
                .unwrap()
                .tickets[0]
                .children
                .is_empty()
        );
        let expected = if legacy {
            vec!["child-2", "ticket"]
        } else {
            vec!["ticket"]
        };
        assert_eq!(*repo.h.removals.borrow(), expected);
        for branch in ["feat/t--1", "feat/t--2", "feat/t"] {
            assert!(
                !git(
                    &repo.main,
                    &["show-ref", "--verify", &format!("refs/heads/{branch}")]
                )
                .status
                .success()
            );
        }
    }
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

#[test]
fn native_cleanup_revalidates_the_whole_set_before_effects() {
    for change in ["dirty", "tip", "marker", "workspace", "record", "live"] {
        let repo = Repo::new();
        let native = recorded_child(&repo, 1, false);
        let legacy = recorded_child(&repo, 2, true);
        repo.merge();
        let preflight = repo.preflight(false).unwrap();
        match change {
            "dirty" => fs::write(native.join("dirty"), "keep").unwrap(),
            "tip" => {
                ok(&native, &["commit", "--allow-empty", "-m", "new work"]);
            }
            "marker" => {
                ok(
                    &native,
                    &["config", "--worktree", "shepherd.ticketBranch", "other"],
                );
            }
            "workspace" => repo.h.workspaces.borrow_mut().push(Workspace {
                workspace_id: "unexpected".into(),
                label: "unexpected".into(),
                worktree: Some(WorktreeInfo {
                    repo_key: "fixture".into(),
                    repo_root: Some(repo.main.clone()),
                    checkout_path: Some(native.clone()),
                    is_linked_worktree: true,
                }),
            }),
            "record" => {
                herdr_shepherd::store::Store::open(&repo.state, &repo.main)
                    .unwrap()
                    .update(|data| {
                        data.tickets[0].children[0].base_sha = "changed".into();
                        Ok(())
                    })
                    .unwrap();
            }
            "live" => {
                let link = repo._dir.path().join("live-cwd");
                std::os::unix::fs::symlink(&native, &link).unwrap();
                repo.h.agents.borrow_mut().push(AgentInfo {
                    name: Some("native-coder".into()),
                    agent: "fake".into(),
                    agent_status: "working".into(),
                    cwd: Some(link),
                    pane_id: "ticket:p1".into(),
                    workspace_id: Some("ticket".into()),
                });
            }
            _ => unreachable!(),
        }
        assert!(cleanup::run(preflight, false).is_err(), "accepted {change}");
        assert!(native.exists(), "removed native after {change}");
        assert!(legacy.exists(), "removed legacy after {change}");
        assert!(repo.ticket.exists());
        assert!(repo.h.removals.borrow().is_empty());
        assert!(
            git(
                &repo.main,
                &["show-ref", "--verify", "refs/heads/feat/t--1"]
            )
            .status
            .success()
        );
    }
}

#[test]
fn force_discard_requires_recorded_native_ownership_and_containment() {
    for unsafe_work in ["unrecorded", "unguarded", "uncontained", "dirty"] {
        let repo = Repo::new();
        let native = recorded_child(&repo, 1, false);
        repo.merge();
        match unsafe_work {
            "unrecorded" => herdr_shepherd::store::Store::open(&repo.state, &repo.main)
                .unwrap()
                .update(|data| {
                    data.tickets[0].children.clear();
                    Ok(())
                })
                .unwrap(),
            "unguarded" => {
                ok(
                    &native,
                    &["config", "--worktree", "--unset", "shepherd.ticketBranch"],
                );
            }
            "uncontained" => {
                fs::write(native.join("unmerged"), "keep").unwrap();
                ok(&native, &["add", "unmerged"]);
                ok(&native, &["commit", "-m", "unmerged work"]);
            }
            "dirty" => fs::write(native.join("dirty"), "discard").unwrap(),
            _ => unreachable!(),
        }
        let result = cleanup::preflight_with_transport(
            &repo.h,
            &repo.backend,
            &repo.cfg,
            &repo.ticket,
            true,
            false,
            repo.transport(),
        );
        if unsafe_work == "dirty" {
            cleanup::run(result.unwrap(), false).unwrap();
            assert!(!native.exists());
            assert!(!repo.ticket.exists());
        } else {
            assert!(result.is_err(), "accepted {unsafe_work}");
            assert!(native.exists());
            assert!(repo.ticket.exists());
            assert!(repo.h.removals.borrow().is_empty());
        }
    }
}

#[test]
fn locked_native_removal_keeps_ticket_and_recovery_records() {
    let repo = Repo::new();
    let native = recorded_child(&repo, 1, false);
    repo.merge();
    let preflight = repo.preflight(false).unwrap();
    ok(&repo.main, &["worktree", "lock", native.to_str().unwrap()]);
    assert!(cleanup::run(preflight, false).is_err());
    assert!(native.exists());
    assert!(repo.ticket.exists());
    assert!(repo.h.removals.borrow().is_empty());
    assert_eq!(
        herdr_shepherd::store::Store::open(&repo.state, &repo.main)
            .unwrap()
            .read()
            .unwrap()
            .tickets[0]
            .children
            .len(),
        1
    );
    assert!(
        git(
            &repo.main,
            &["show-ref", "--verify", "refs/heads/feat/t--1"]
        )
        .status
        .success()
    );
}

#[test]
fn recorded_child_branch_reassignment_blocks_parent_cleanup() {
    let repo = Repo::new();
    let native = recorded_child(&repo, 1, false);
    repo.merge();
    ok(&native, &["checkout", "-b", "unrelated"]);
    assert!(repo.preflight(false).is_err());
    assert!(native.exists());
    assert!(repo.ticket.exists());
    assert!(repo.h.removals.borrow().is_empty());
}

#[test]
fn exact_recorded_child_agent_blocks_cleanup_without_live_cwd() {
    let repo = Repo::new();
    let native = recorded_child(&repo, 1, false);
    repo.merge();
    herdr_shepherd::store::Store::open(&repo.state, &repo.main)
        .unwrap()
        .update(|data| {
            data.agents.push(herdr_shepherd::store::AgentRecord {
                name: "native-coder".into(),
                ticket: "T".into(),
                role: "coder".into(),
                runner: "fake".into(),
                status: herdr_shepherd::store::AgentStatus::Working,
                pane_id: Some("ticket:p1".into()),
                tab_id: Some("ticket:t1".into()),
                worktree: native.clone(),
                task: "work".into(),
                report: None,
                observed: None,
                last_progress_at: "0".into(),
                nudges: 0,
                restarts: 0,
                prompt_pending: false,
                task_id: None,
                attempt_id: None,
                unit: None,
                launch_profile: None,
                completed_tasks: vec![],
                dispatch_reason: None,
            });
            Ok(())
        })
        .unwrap();
    repo.h.agents.borrow_mut().push(AgentInfo {
        name: Some("native-coder".into()),
        agent: "fake".into(),
        agent_status: "working".into(),
        cwd: None,
        pane_id: "ticket:p1".into(),
        workspace_id: None,
    });
    assert!(repo.preflight(false).is_err());
    assert!(native.exists());
    assert!(repo.ticket.exists());
    assert!(repo.h.removals.borrow().is_empty());
}

#[test]
fn concurrent_registration_between_removals_keeps_ticket_and_new_child() {
    let repo = Repo::new();
    recorded_child(&repo, 1, true);
    repo.merge();
    let preflight = repo.preflight(false).unwrap();
    let main = repo.main.clone();
    let path = repo._dir.path().join("new-child");
    let new_child = path.clone();
    let store = herdr_shepherd::store::Store::open(&repo.state, &repo.main).unwrap();
    let current = store.clone();
    *repo.h.on_remove.borrow_mut() = Some(Box::new(move || {
        ok(
            &main,
            &[
                "worktree",
                "add",
                "-b",
                "feat/t--2",
                path.to_str().unwrap(),
                "feat/t",
            ],
        );
        ok(
            &path,
            &["config", "--worktree", "shepherd.ticketBranch", "feat/t--2"],
        );
        store
            .update(|data| {
                data.tickets[0]
                    .children
                    .push(herdr_shepherd::store::ChildWorktree {
                        unit: 2,
                        branch: "feat/t--2".into(),
                        path: path.clone(),
                        workspace_id: None,
                        base_sha: ok(&path, &["rev-parse", "HEAD"]).trim().into(),
                        integrated: false,
                    });
                Ok(())
            })
            .unwrap();
    }));
    assert!(cleanup::run(preflight, false).is_err());
    assert!(repo.ticket.exists());
    assert!(new_child.exists());
    assert_eq!(*repo.h.removals.borrow(), ["child-1"]);
    assert!(
        current.read().unwrap().tickets[0]
            .children
            .iter()
            .any(|child| child.unit == 2)
    );
    assert!(
        git(
            &repo.main,
            &["show-ref", "--verify", "refs/heads/feat/t--2"]
        )
        .status
        .success()
    );
}

fn assert_cleanup_finished(repo: &Repo) {
    assert!(!repo.ticket.exists());
    let store = herdr_shepherd::store::Store::open(&repo.state, &repo.main).unwrap();
    let data = store.read().unwrap();
    assert!(data.tickets[0].children.is_empty());
    assert!(data.tickets[0].child_cleanup.is_empty());
    for branch in ["feat/t--1", "feat/t--2", "feat/t"] {
        assert!(
            !git(
                &repo.main,
                &["show-ref", "--verify", &format!("refs/heads/{branch}")]
            )
            .status
            .success()
        );
    }
    assert_eq!(repo.h.workspaces.borrow().len(), 1);
    assert_eq!(
        ok(&repo.main, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
}

#[test]
fn partial_child_cleanup_can_retry_after_unlock() {
    for (first_legacy, second_legacy) in
        [(false, false), (false, true), (true, false), (true, true)]
    {
        let repo = Repo::new();
        let first = recorded_child(&repo, 1, first_legacy);
        let second = recorded_child(&repo, 2, second_legacy);
        repo.merge();
        ok(&repo.main, &["worktree", "lock", second.to_str().unwrap()]);
        let error = cleanup::run(repo.preflight(false).unwrap(), false).unwrap_err();
        assert!(error.to_string().contains("locked"), "{error}");
        assert!(!first.exists());
        assert!(second.exists());
        assert!(repo.ticket.exists());
        ok(
            &repo.main,
            &["worktree", "unlock", second.to_str().unwrap()],
        );
        let retry = repo
            .preflight(false)
            .unwrap_or_else(|error| panic!("retry preflight: {error}"));
        cleanup::run(retry, false).unwrap();
        assert!(!second.exists());
        assert_cleanup_finished(&repo);
    }
}

#[test]
fn partial_cleanup_retries_child_ref_deletion() {
    for legacy in [false, true] {
        let repo = Repo::new();
        let first = recorded_child(&repo, 1, legacy);
        let second = recorded_child(&repo, 2, !legacy);
        repo.merge();
        let lock = repo.main.join(".git/refs/heads/feat/t--1.lock");
        fs::write(&lock, "locked").unwrap();
        assert!(cleanup::run(repo.preflight(false).unwrap(), false).is_err());
        assert!(!first.exists());
        assert!(repo.ticket.exists());
        fs::remove_file(lock).unwrap();
        let store = herdr_shepherd::store::Store::open(&repo.state, &repo.main).unwrap();
        let before = store.read().unwrap();
        assert_eq!(before.tickets[0].child_cleanup.len(), 1);
        assert_eq!(before.tickets[0].child_cleanup[0].child.unit, 1);
        assert_eq!(before.tickets[0].child_cleanup[0].path, first);
        let preview = cleanup::run(repo.preflight(true).unwrap(), true).unwrap();
        assert!(preview.children_removed.contains(&first));
        assert_eq!(store.read().unwrap().tickets, before.tickets);
        assert!(second.exists());
        if legacy {
            // Simulate interruption after ref deletion but before store reconciliation.
            ok(&repo.main, &["update-ref", "-d", "refs/heads/feat/t--1"]);
        }
        cleanup::run(repo.preflight(false).unwrap(), false).unwrap();
        assert!(!second.exists());
        assert_cleanup_finished(&repo);
    }
}

#[test]
fn partial_cleanup_retries_before_ticket_workspace_removal() {
    let repo = Repo::new();
    let first = recorded_child(&repo, 1, false);
    let second = recorded_child(&repo, 2, true);
    repo.merge();
    ok(
        &repo.main,
        &["worktree", "lock", repo.ticket.to_str().unwrap()],
    );
    assert!(cleanup::run(repo.preflight(false).unwrap(), false).is_err());
    assert!(!first.exists());
    assert!(!second.exists());
    ok(
        &repo.main,
        &["worktree", "unlock", repo.ticket.to_str().unwrap()],
    );
    cleanup::run(repo.preflight(false).unwrap(), false).unwrap();
    assert_cleanup_finished(&repo);
}

#[test]
fn partial_cleanup_retries_legacy_workspace_close() {
    let repo = Repo::new();
    let first = recorded_child(&repo, 1, true);
    let second = recorded_child(&repo, 2, false);
    repo.merge();
    *repo.h.fail_after_checkout_removal.borrow_mut() = true;
    assert!(cleanup::run(repo.preflight(false).unwrap(), false).is_err());
    assert!(!first.exists());
    assert!(
        repo.h
            .workspaces
            .borrow()
            .iter()
            .any(|ws| ws.workspace_id == "child-1")
    );
    cleanup::run(repo.preflight(false).unwrap(), false).unwrap();
    assert!(!second.exists());
    assert_cleanup_finished(&repo);
}

#[test]
fn missing_child_without_cleanup_evidence_is_refused() {
    let repo = Repo::new();
    let first = recorded_child(&repo, 1, false);
    recorded_child(&repo, 2, true);
    repo.merge();
    ok(&repo.main, &["worktree", "remove", first.to_str().unwrap()]);
    assert!(repo.preflight(false).is_err());
    assert!(repo.ticket.exists());
    assert!(repo.h.removals.borrow().is_empty());
}

#[test]
fn partial_cleanup_refuses_changed_resources() {
    for change in [
        "moved-ref",
        "reassigned-checkout",
        "unrelated-directory",
        "symlink",
        "record-path",
        "record-base",
        "record-workspace",
        "record-unit",
        "legacy-workspace",
    ] {
        let repo = Repo::new();
        let first = recorded_child(&repo, 1, true);
        let second = recorded_child(&repo, 2, false);
        repo.merge();
        let lock = repo.main.join(".git/refs/heads/feat/t--1.lock");
        fs::write(&lock, "locked").unwrap();
        assert!(cleanup::run(repo.preflight(false).unwrap(), false).is_err());
        assert!(!first.exists());
        fs::remove_file(lock).unwrap();
        let store = herdr_shepherd::store::Store::open(&repo.state, &repo.main).unwrap();
        match change {
            "moved-ref" => {
                ok(&repo.main, &["update-ref", "refs/heads/feat/t--1", "HEAD^"]);
            }
            "reassigned-checkout" => {
                ok(
                    &repo.main,
                    &[
                        "worktree",
                        "add",
                        "-b",
                        "unrelated",
                        first.to_str().unwrap(),
                    ],
                );
            }
            "unrelated-directory" => {
                fs::create_dir(&first).unwrap();
            }
            "symlink" => {
                std::os::unix::fs::symlink(&second, &first).unwrap();
            }
            "legacy-workspace" => {
                repo.h.workspaces.borrow_mut().push(Workspace {
                    workspace_id: "child-1".into(),
                    label: "reassigned".into(),
                    worktree: Some(WorktreeInfo {
                        repo_key: "fixture".into(),
                        repo_root: Some(repo.main.clone()),
                        checkout_path: Some(second.clone()),
                        is_linked_worktree: true,
                    }),
                });
            }
            _ => {
                store
                    .update(|data| {
                        let child = &mut data.tickets[0].children[0];
                        match change {
                            "record-path" => child.path = repo._dir.path().join("tampered"),
                            "record-base" => child.base_sha = "tampered".into(),
                            "record-workspace" => child.workspace_id = None,
                            "record-unit" => child.unit = 99,
                            _ => unreachable!(),
                        }
                        Ok(())
                    })
                    .unwrap();
            }
        }
        assert!(repo.preflight(false).is_err(), "accepted {change}");
        assert!(repo.ticket.exists());
        assert!(second.exists(), "removed remaining child after {change}");
    }
}

#[test]
fn tickets_without_child_cleanup_intent_still_load() {
    for legacy in [false, true] {
        let repo = Repo::new();
        recorded_child(&repo, 1, legacy);
        let store = herdr_shepherd::store::Store::open(&repo.state, &repo.main).unwrap();
        let ticket = store.read().unwrap().tickets.remove(0);
        let mut json = serde_json::to_value(&ticket).unwrap();
        json.as_object_mut().unwrap().remove("child_cleanup");
        let loaded: herdr_shepherd::store::Ticket = serde_json::from_value(json).unwrap();
        assert_eq!(loaded, ticket);
        assert!(loaded.child_cleanup.is_empty());
    }
}

#[test]
fn integration_refuses_final_cleanup_intent_before_mutation() {
    let repo = Repo::new();
    let native = recorded_child(&repo, 1, false);
    herdr_shepherd::store::Store::open(&repo.state, &repo.main)
        .unwrap()
        .update(|data| {
            data.tickets[0].children[0].integrated = false;
            Ok(())
        })
        .unwrap();
    repo.merge();
    ok(&repo.main, &["worktree", "lock", native.to_str().unwrap()]);
    let error = cleanup::run(repo.preflight(false).unwrap(), false).unwrap_err();
    assert!(error.to_string().contains("locked"), "{error}");
    let store = herdr_shepherd::store::Store::open(&repo.state, &repo.main).unwrap();
    let before = store.read().unwrap().tickets.remove(0);
    assert!(!before.children[0].integrated);
    assert_eq!(before.child_cleanup[0].child, before.children[0]);
    assert_eq!(
        ok(&native, &["rev-parse", "HEAD"]),
        format!("{}\n", before.children[0].base_sha)
    );
    let head = ok(&repo.ticket, &["rev-parse", "HEAD"]);
    let fake = repo._dir.path().join("fake.json");
    fs::write(&fake, "{}").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .env("SHEPHERD_FAKE_HERDR", fake)
        .args(["--json", "--state-dir"])
        .arg(&repo.state)
        .arg("--config-dir")
        .arg(repo._dir.path().join("config"))
        .args(["ticket", "--project-root"])
        .arg(&repo.main)
        .args(["integrate", "T"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        message.contains("finish the outstanding cleanup"),
        "{message}"
    );
    assert_eq!(store.read().unwrap().tickets[0], before);
    assert_eq!(ok(&repo.ticket, &["rev-parse", "HEAD"]), head);
    ok(
        &repo.main,
        &["worktree", "unlock", native.to_str().unwrap()],
    );
    cleanup::run(repo.preflight(false).unwrap(), false).unwrap();
    assert_cleanup_finished(&repo);
}

fn reported_cleanup_agent(repo: &Repo, role: &str, number: u32) {
    use herdr_shepherd::store::{AgentRecord, AgentStatus, Store};
    let pane = format!("ticket:p{number}");
    let session = format!("session-{number}");
    let name = format!("reused-{role}");
    let task = format!("{role}-revision");
    let store = Store::open(&repo.state, &repo.main).unwrap();
    store
        .update(|data| {
            data.agents.push(AgentRecord {
                name: name.clone(),
                ticket: "T".into(),
                role: role.into(),
                runner: "codex-sol-high".into(),
                status: AgentStatus::Working,
                pane_id: Some(pane.clone()),
                tab_id: Some(format!("ticket:t{number}")),
                worktree: repo.ticket.clone(),
                task: format!("verify {role} revision"),
                report: None,
                observed: Some(serde_json::json!({"session_ref":session}).to_string()),
                last_progress_at: "0".into(),
                nudges: 0,
                restarts: 0,
                prompt_pending: false,
                task_id: Some(task.clone()),
                attempt_id: Some(format!("{task}-1")),
                unit: None,
                launch_profile: None,
                completed_tasks: vec![],
                dispatch_reason: Some("reused: eligible reported agent".into()),
            });
            Ok(())
        })
        .unwrap();
    herdr_shepherd::flock::store_report_for_task(
        &store, &name, Some(&task),
        r#"{"status":"completed","summary":"revision complete","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#,
    ).unwrap();
    repo.h.agents.borrow_mut().push(AgentInfo {
        name: Some(name),
        agent: "codex".into(),
        agent_status: "idle".into(),
        cwd: Some(repo.ticket.clone()),
        pane_id: pane.clone(),
        workspace_id: Some("ticket".into()),
    });
    repo.h.pane_sessions.borrow_mut().insert(pane, session);
}

fn retirement_hook(repo: &mut Repo) -> PathBuf {
    let events = repo._dir.path().join("retirement-events.jsonl");
    let command: toml::Value = vec![
        "sh".to_owned(),
        "-c".into(),
        "cat >> \"$1\"; printf '\\n' >> \"$1\"".into(),
        "hook".into(),
        events.display().to_string(),
    ]
    .into();
    repo.cfg.raw.insert(
        "hooks".into(),
        toml::Value::Array(vec![toml::Value::Table(toml::Table::from_iter([
            ("on".into(), "agent.retired".into()),
            ("command".into(), command),
        ]))]),
    );
    events
}

#[test]
fn cleanup_retires_owned_reported_agents_before_removal() {
    use herdr_shepherd::store::{AgentStatus, Store};
    let mut repo = Repo::new();
    let native = recorded_child(&repo, 1, false);
    repo.merge();
    let events = retirement_hook(&mut repo);
    for (number, role) in [(1, "coder"), (2, "reviewer"), (3, "qa")] {
        reported_cleanup_agent(&repo, role, number);
    }
    let store = Store::open(&repo.state, &repo.main).unwrap();
    store
        .update(|data| {
            data.agents[0].worktree = native.clone();
            data.agents[0].unit = Some(1);
            Ok(())
        })
        .unwrap();
    repo.h.agents.borrow_mut()[0].cwd = Some(native.clone());
    let before = store.read().unwrap().agents;
    let preflight = repo
        .preflight(false)
        .expect("owned idle reported agents should be retired by cleanup");
    assert_eq!(store.read().unwrap().agents, before);
    assert!(repo.h.closed_tabs.borrow().is_empty());
    assert!(!events.exists());
    let report = cleanup::run(preflight, false).unwrap();
    assert!(report.workspace_removed);
    assert!(!repo.ticket.exists());
    assert!(repo.h.agents.borrow().is_empty());
    assert!(repo.h.pane_sessions.borrow().is_empty());
    assert_eq!(repo.h.closed_tabs.borrow().len(), 3);
    assert!(!native.exists());
    let emitted: Vec<serde_json::Value> = fs::read_to_string(events)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(emitted.len(), 3);
    for (index, role) in ["coder", "reviewer", "qa"].into_iter().enumerate() {
        assert_eq!(emitted[index]["role"], role);
        assert_eq!(emitted[index]["task_id"], format!("{role}-revision"));
    }
    for (old, current) in before.iter().zip(store.read().unwrap().agents) {
        let mut expected = old.clone();
        expected.status = AgentStatus::Retired;
        assert_eq!(current, expected);
    }
}

#[test]
fn dry_run_previews_reported_retirement_without_effects() {
    use herdr_shepherd::store::Store;
    let mut repo = Repo::new();
    repo.merge();
    let events = retirement_hook(&mut repo);
    reported_cleanup_agent(&repo, "reviewer", 1);
    let store = Store::open(&repo.state, &repo.main).unwrap();
    let before = store.read().unwrap().agents;
    let refs = ok(&repo.main, &["for-each-ref"]);
    let report = cleanup::run(
        repo.preflight(true)
            .expect("idle agents should permit preview"),
        true,
    )
    .unwrap();
    assert!(report.dry_run);
    assert_eq!(store.read().unwrap().agents, before);
    assert_eq!(ok(&repo.main, &["for-each-ref"]), refs);
    assert_eq!(repo.h.agents.borrow().len(), 1);
    assert_eq!(repo.h.pane_sessions.borrow().len(), 1);
    assert!(repo.h.closed_tabs.borrow().is_empty());
    assert!(repo.h.removals.borrow().is_empty());
    assert!(repo.h.notifications.borrow().is_empty());
    assert!(!events.exists());
    assert!(repo.ticket.exists());
}

#[test]
fn cleanup_requires_physical_reported_process_shutdown() {
    use herdr_shepherd::store::{AgentStatus, Store};
    let repo = Repo::new();
    repo.merge();
    reported_cleanup_agent(&repo, "coder", 1);
    *repo.h.close_tab_effects.borrow_mut() = false;
    let preflight = repo
        .preflight(false)
        .expect("verified idle agent is a retirement candidate");
    assert!(cleanup::run(preflight, false).is_err());
    assert!(repo.ticket.exists());
    assert!(repo.h.removals.borrow().is_empty());
    assert_eq!(repo.h.closed_tabs.borrow().len(), 1);
    assert_eq!(repo.h.agents.borrow().len(), 1);
    assert_eq!(
        Store::open(&repo.state, &repo.main)
            .unwrap()
            .read()
            .unwrap()
            .agents[0]
            .status,
        AgentStatus::Reported
    );
}

#[test]
fn cleanup_refuses_unverified_reported_recipients() {
    use herdr_shepherd::store::{PromptClaim, Store};
    for change in [
        "pane",
        "session",
        "workspace",
        "working",
        "uncertain",
        "claim",
        "missing-session",
    ] {
        let repo = Repo::new();
        repo.merge();
        reported_cleanup_agent(&repo, "coder", 1);
        match change {
            "pane" => repo.h.agents.borrow_mut()[0].pane_id = "ticket:other".into(),
            "session" => {
                repo.h
                    .pane_sessions
                    .borrow_mut()
                    .insert("ticket:p1".into(), "replacement".into());
            }
            "workspace" => repo.h.agents.borrow_mut()[0].workspace_id = Some("unowned".into()),
            "working" => repo.h.agents.borrow_mut()[0].agent_status = "working".into(),
            "uncertain" | "claim" | "missing-session" => {
                Store::open(&repo.state, &repo.main)
                    .unwrap()
                    .update(|data| {
                        let rec = &mut data.agents[0];
                        if change == "uncertain" {
                            rec.set_prompt_delivery_uncertain(true);
                        } else if change == "claim" {
                            let claim = PromptClaim {
                                task_id: rec.task_id.clone(),
                                attempt_id: rec.attempt_id.clone(),
                                kind: "assignment".into(),
                                text: "possibly pending".into(),
                                queued_ping: None,
                            };
                            let mut observed: serde_json::Value =
                                serde_json::from_str(rec.observed.as_deref().unwrap()).unwrap();
                            observed["prompt_claim"] = serde_json::to_value(claim).unwrap();
                            rec.observed = Some(observed.to_string());
                        } else {
                            rec.observed = None;
                        }
                        Ok(())
                    })
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(repo.preflight(false).is_err(), "accepted {change}");
        assert!(repo.h.closed_tabs.borrow().is_empty(), "closed {change}");
        assert!(repo.h.removals.borrow().is_empty());
        assert!(repo.ticket.exists());
    }
}

#[test]
fn cleanup_rechecks_planned_reported_agent_before_closing() {
    for change in ["record", "session", "working", "new-agent"] {
        let repo = Repo::new();
        repo.merge();
        reported_cleanup_agent(&repo, "coder", 1);
        let preflight = repo
            .preflight(false)
            .expect("owned idle recipient should be planned");
        match change {
            "record" => herdr_shepherd::store::Store::open(&repo.state, &repo.main)
                .unwrap()
                .update(|data| {
                    data.agents[0].attempt_id = Some("replacement-attempt".into());
                    Ok(())
                })
                .unwrap(),
            "session" => {
                repo.h
                    .pane_sessions
                    .borrow_mut()
                    .insert("ticket:p1".into(), "replacement".into());
            }
            "working" => repo.h.agents.borrow_mut()[0].agent_status = "working".into(),
            "new-agent" => {
                let mut live = repo.h.agents.borrow()[0].clone();
                live.name = Some("unowned".into());
                live.pane_id = "ticket:other".into();
                repo.h.agents.borrow_mut().push(live);
            }
            _ => unreachable!(),
        }
        assert!(cleanup::run(preflight, false).is_err(), "accepted {change}");
        assert!(repo.h.closed_tabs.borrow().is_empty(), "closed {change}");
        assert!(repo.h.removals.borrow().is_empty());
        assert!(repo.ticket.exists());
    }
}

#[test]
fn cleanup_keeps_actual_no_agents_check_after_reported_retirement() {
    use herdr_shepherd::store::{AgentStatus, Store};
    let repo = Repo::new();
    repo.merge();
    reported_cleanup_agent(&repo, "coder", 1);
    let preflight = repo.preflight(false).unwrap();
    *repo.h.agent_after_close.borrow_mut() = Some(AgentInfo {
        name: Some("unowned-late-process".into()),
        agent: "codex".into(),
        agent_status: "working".into(),
        cwd: Some(repo.ticket.clone()),
        pane_id: "ticket:other".into(),
        workspace_id: Some("ticket".into()),
    });
    assert!(cleanup::run(preflight, false).is_err());
    assert!(repo.ticket.exists());
    assert!(repo.h.removals.borrow().is_empty());
    assert_eq!(repo.h.closed_tabs.borrow().len(), 1);
    assert_eq!(repo.h.agents.borrow().len(), 1);
    assert_eq!(
        repo.h.agents.borrow()[0].name.as_deref(),
        Some("unowned-late-process")
    );
    assert_eq!(
        Store::open(&repo.state, &repo.main)
            .unwrap()
            .read()
            .unwrap()
            .agents[0]
            .status,
        AgentStatus::Retired
    );
}

#[test]
fn cleanup_finishes_pending_ci_and_revision_assignments() {
    use herdr_shepherd::{
        store::{Store, TicketState},
        ticket::{
            revise::{AssignmentRef, CiRecovery, RevisionRun, RevisionStage},
            scheduler,
        },
    };
    for state in [TicketState::Building, TicketState::Revising] {
        for outcome in ["success", "dry-run", "shutdown-failed"] {
            let repo = Repo::new();
            repo.merge();
            reported_cleanup_agent(&repo, "coder", 1);
            let store = Store::open(&repo.state, &repo.main).unwrap();
            store
                .update(|data| {
                    let assignment = AssignmentRef::from_agent(&data.agents[0]);
                    let ticket = &mut data.tickets[0];
                    ticket.state = state;
                    ticket.ci_failure_task = Some("fix CI".into());
                    ticket.ci_recovery = Some(CiRecovery {
                        generation: "generation".into(),
                        assignment: assignment.clone(),
                    });
                    ticket.revision_task = Some("fix review".into());
                    ticket.revision_run = Some(RevisionRun {
                        base_sha: "base".into(),
                        synced_sha: None,
                        stage: RevisionStage::Coder,
                        agent: Some(assignment.agent),
                        task_id: assignment.task_id,
                        attempt_id: assignment.attempt_id,
                        tested_sha: None,
                    });
                    Ok(())
                })
                .unwrap();
            let before = store.read().unwrap().tickets[0].clone();
            let dry_run = outcome == "dry-run";
            let preflight = repo.preflight(dry_run).unwrap();
            if outcome == "shutdown-failed" {
                *repo.h.close_tab_effects.borrow_mut() = false;
                assert!(cleanup::run(preflight, false).is_err());
            } else {
                cleanup::run(preflight, dry_run).unwrap();
            }
            let data = store.read().unwrap();
            let ticket = &data.tickets[0];
            if outcome != "success" {
                assert_eq!(
                    *ticket, before,
                    "{state:?} {outcome} must preserve dispatch intent"
                );
                continue;
            }
            let settings = herdr_shepherd::roles::resolve(&repo.cfg).unwrap().shepherd;
            assert!(
                scheduler::next_actions(&data, &settings).is_empty(),
                "removed checkout must never receive another CI/revision assignment"
            );
            assert_eq!(ticket.state, TicketState::CleanedUp);
            assert!(ticket.ci_failure_task.is_none());
            assert!(ticket.ci_recovery.is_none());
            assert!(ticket.revision_task.is_none());
            assert!(ticket.revision_run.is_none());
            assert_eq!(ticket.history.last().unwrap().to, TicketState::CleanedUp);
        }
    }
}

#[test]
fn cleanup_reconciles_retirement_after_lost_close_acknowledgement() {
    use herdr_shepherd::store::{AgentStatus, Store};
    for failed_close in [1, 2] {
        let mut repo = Repo::new();
        repo.merge();
        let events = retirement_hook(&mut repo);
        reported_cleanup_agent(&repo, "coder", 1);
        reported_cleanup_agent(&repo, "reviewer", 2);
        let store = Store::open(&repo.state, &repo.main).unwrap();
        repo.h.fail_after_tab_close.replace(Some(failed_close));
        let error = cleanup::run(repo.preflight(false).unwrap(), false).unwrap_err();
        assert!(error.to_string().contains("lost close acknowledgement"));
        assert!(repo.ticket.exists());
        assert_eq!(repo.h.agents.borrow().len(), 2 - failed_close);
        let state = store.read().unwrap();
        assert_eq!(state.agents[failed_close - 1].status, AgentStatus::Reported);
        // The entire set is durable before even the first close.
        assert_eq!(state.cleanup_retiring_agents.len(), 2);
        let emitted_before = fs::read_to_string(&events).unwrap_or_default();
        assert_eq!(emitted_before.lines().count(), failed_close - 1);

        // Previewing the retry must not reconcile state or emit retirement events.
        let before = fs::read(store.root().join("state.json")).unwrap();
        cleanup::run(repo.preflight(true).unwrap(), true).unwrap();
        assert_eq!(fs::read(store.root().join("state.json")).unwrap(), before);
        assert_eq!(
            fs::read_to_string(&events).unwrap_or_default(),
            emitted_before
        );

        cleanup::run(repo.preflight(false).unwrap(), false).unwrap();
        assert!(!repo.ticket.exists());
        assert!(repo.h.agents.borrow().is_empty());
        assert_eq!(repo.h.closed_tabs.borrow().len(), 2);
        assert!(store.read().unwrap().cleanup_retiring_agents.is_empty());
        assert!(
            store
                .read()
                .unwrap()
                .agents
                .iter()
                .all(|agent| agent.status == AgentStatus::Retired)
        );
        let emitted: Vec<serde_json::Value> = fs::read_to_string(events)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(emitted.len(), 2);
        assert_eq!(emitted[0]["role"], "coder");
        assert_eq!(emitted[1]["role"], "reviewer");
    }
}

#[test]
fn cleanup_rejects_changed_journaled_recipient() {
    use herdr_shepherd::store::Store;
    for changed in ["task", "attempt", "pane", "tab", "session"] {
        let repo = Repo::new();
        repo.merge();
        reported_cleanup_agent(&repo, "coder", 1);
        let store = Store::open(&repo.state, &repo.main).unwrap();
        repo.h.fail_after_tab_close.replace(Some(1));
        assert!(cleanup::run(repo.preflight(false).unwrap(), false).is_err());
        store
            .update(|data| {
                let rec = &mut data.agents[0];
                match changed {
                    "task" => rec.task_id = Some("replacement-task".into()),
                    "attempt" => rec.attempt_id = Some("replacement-attempt".into()),
                    "pane" => rec.pane_id = Some("ticket:replacement-pane".into()),
                    "tab" => rec.tab_id = Some("ticket:replacement-tab".into()),
                    "session" => {
                        rec.observed = Some(r#"{"session_ref":"replacement-session"}"#.into());
                    }
                    _ => unreachable!(),
                }
                Ok(())
            })
            .unwrap();
        let result = repo.preflight(false);
        assert!(result.is_err());
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("retirement intent changed")
        );
        assert!(repo.ticket.exists());
        assert_eq!(repo.h.closed_tabs.borrow().len(), 1);
        assert!(repo.h.removals.borrow().is_empty());
    }
}

#[test]
fn cleanup_pending_retirement_fences_new_assignments() {
    use herdr_shepherd::{
        flock,
        launch::LaunchRequest,
        roles,
        store::{Store, TicketState},
    };
    let repo = Repo::new();
    repo.merge();
    reported_cleanup_agent(&repo, "coder", 1);
    let store = Store::open(&repo.state, &repo.main).unwrap();
    store
        .update(|data| {
            data.tickets[0].state = TicketState::Building;
            Ok(())
        })
        .unwrap();
    repo.h.fail_after_tab_close.replace(Some(1));
    assert!(cleanup::run(repo.preflight(false).unwrap(), false).is_err());
    let before = store.read().unwrap();
    let resolved = roles::resolve(&repo.cfg).unwrap();
    for role in ["coder", "reviewer", "qa"] {
        let request = LaunchRequest {
            ticket: "T".into(),
            role: resolved
                .roles
                .iter()
                .find(|candidate| candidate.name == role)
                .unwrap()
                .clone(),
            runners: vec![resolved.runners["codex-sol-high"].clone()],
            worktree: repo.ticket.clone(),
            workspace_id: "ticket".into(),
            tab_label: role.into(),
            prompt: "Do the next assignment".into(),
            state_dir: repo.state.clone(),
        };
        let error = flock::spawn(&repo.h, &store, &repo.cfg, request).unwrap_err();
        assert!(error.to_string().contains("cleanup"), "{role}: {error}");
        assert_eq!(store.read().unwrap(), before);
        assert!(repo.ticket.exists());
    }
}
